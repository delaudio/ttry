use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ttry::runner::{run_config, Config, Reporter, RunOptions, TestStatus};

#[derive(Debug, Parser)]
#[command(
    name = "ttry",
    version,
    about = "End-to-end tests for terminal user interfaces"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Discover and run tests from a ttry TOML configuration.
    Test {
        #[arg(short, long, default_value = "ttry.toml")]
        config: PathBuf,
        #[arg(long)]
        grep: Option<String>,
        #[arg(long, value_name = "MILLISECONDS", value_parser = parse_positive_timeout)]
        timeout: Option<u64>,
        #[arg(long, value_parser = parse_reporter)]
        reporter: Option<Reporter>,
        /// Enable snapshot creation/update for test code that reads this environment flag.
        #[arg(short = 'u', long)]
        update_snapshots: bool,
    },
}

fn parse_reporter(value: &str) -> Result<Reporter, String> {
    value.parse()
}

fn parse_positive_timeout(value: &str) -> Result<u64, String> {
    let timeout = value
        .parse::<u64>()
        .map_err(|_| format!("invalid timeout `{value}`; expected milliseconds"))?;
    if timeout == 0 {
        Err("timeout must be greater than zero".into())
    } else {
        Ok(timeout)
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Test {
            config,
            grep,
            timeout,
            reporter,
            update_snapshots,
        } => {
            if update_snapshots {
                std::env::set_var("TTRY_UPDATE_SNAPSHOTS", "1");
            }
            let config = match Config::load(&config) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    return ExitCode::from(2);
                }
            };
            let selected_reporter =
                reporter.unwrap_or_else(|| config.reporter.parse().unwrap_or_default());
            let report = run_config(
                config,
                RunOptions {
                    grep,
                    timeout: timeout.map(Duration::from_millis),
                    reporter: Some(selected_reporter),
                },
            );
            match selected_reporter {
                Reporter::List => {
                    for result in &report.tests {
                        match &result.status {
                            TestStatus::Passed => {
                                println!("PASS {} ({:?})", result.name, result.duration)
                            }
                            TestStatus::Skipped => println!("SKIP {}", result.name),
                            TestStatus::Failed(error) => {
                                println!("FAIL {}\n  {}", result.name, error.replace('\n', "\n  "))
                            }
                        }
                    }
                }
                Reporter::Dot => {
                    for result in &report.tests {
                        print!(
                            "{}",
                            match &result.status {
                                TestStatus::Passed => '.',
                                TestStatus::Skipped => 's',
                                TestStatus::Failed(_) => 'F',
                            }
                        );
                    }
                    println!();
                }
            }
            println!("{}", report.summary());
            if report.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

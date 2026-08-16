use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ttry::runner::{run_config, Config, Reporter, RunOptions, TestStatus, MAX_TIMEOUT_MS};

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
        /// Override the suite default timeout; explicit per-test timeouts still take precedence.
        #[arg(long, value_name = "MILLISECONDS", value_parser = parse_positive_timeout)]
        timeout: Option<u64>,
        #[arg(long, value_parser = parse_reporter)]
        reporter: Option<Reporter>,
        /// Enable snapshot creation/update for the selected test run.
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
    if !(1..=MAX_TIMEOUT_MS).contains(&timeout) {
        Err(format!(
            "timeout must be between 1 and {MAX_TIMEOUT_MS} milliseconds"
        ))
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
            let config = match Config::load(&config) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    return ExitCode::from(2);
                }
            };
            let report = run_config(
                config,
                RunOptions {
                    grep,
                    timeout: timeout.map(Duration::from_millis),
                    reporter,
                    update_snapshots,
                },
            );
            match &report.reporter {
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
                    println!("{}", report.summary());
                }
                Reporter::Dot => {
                    let stdout = std::io::stdout();
                    let mut stdout = stdout.lock();
                    for result in &report.tests {
                        if write!(
                            stdout,
                            "{}",
                            match &result.status {
                                TestStatus::Passed => '.',
                                TestStatus::Skipped => 's',
                                TestStatus::Failed(_) => 'F',
                            }
                        )
                        .is_err()
                        {
                            eprintln!("error: could not write dot reporter output");
                            return ExitCode::from(2);
                        }
                    }
                    if writeln!(stdout).and_then(|()| stdout.flush()).is_err() {
                        eprintln!("error: could not flush dot reporter output");
                        return ExitCode::from(2);
                    }
                    for result in &report.tests {
                        if let TestStatus::Failed(error) = &result.status {
                            eprintln!("FAIL {}\n  {}", result.name, error.replace('\n', "\n  "));
                        }
                    }
                    // Keep stdout machine-stable for compact progress parsers.
                    eprintln!("{}", report.summary());
                }
            }
            if report.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

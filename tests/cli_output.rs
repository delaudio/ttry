#![cfg(unix)]

use std::fs;
use std::process::Command;

#[test]
fn dot_reporter_keeps_stdout_machine_stable() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("ttry.toml");
    fs::write(
        &config,
        "reporter = 'dot'\n[[tests]]\nname = 'passes'\ncommand = '/bin/sh'\nargs = ['-c', 'exit 0']\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ttry"))
        .args(["test", "--config"])
        .arg(config)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), ".\n");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "1 passed, 0 failed, 0 skipped\n"
    );
}

#[test]
fn dot_reporter_writes_failure_diagnostics_to_stderr() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("ttry.toml");
    fs::write(
        &config,
        "reporter = 'dot'\n[[tests]]\nname = 'fails'\ncommand = '/bin/sh'\nargs = ['-c', 'exit 7']\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ttry"))
        .args(["test", "--config"])
        .arg(config)
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "F\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("FAIL fails"), "{stderr}");
    assert!(stderr.contains("exit code 7"), "{stderr}");
    assert!(stderr.contains("0 passed, 1 failed, 0 skipped"), "{stderr}");
}

#[test]
fn cli_rejects_timeout_values_that_can_overflow_deadlines() {
    let output = Command::new(env!("CARGO_BIN_EXE_ttry"))
        .args(["test", "--timeout", "86400001"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("timeout must be between 1 and 86400000 milliseconds"));
}

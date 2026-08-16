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

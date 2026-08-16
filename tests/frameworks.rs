use std::path::PathBuf;
use std::time::Duration;

use ttry::{LaunchOptions, TuiSession};

#[test]
#[ignore = "requires the isolated Ratatui fixture; CI sets TTRY_RATATUI_BIN"]
fn ratatui_smoke() {
    let binary = required_env("TTRY_RATATUI_BIN");
    let session = TuiSession::launch(LaunchOptions::new(binary).size(50, 10)).unwrap();
    session
        .wait_for_text("Ratatui ready", Duration::from_secs(5))
        .unwrap();
    session.keyboard().press("x").unwrap();
    session
        .wait_for_text("Ratatui key: x", Duration::from_secs(5))
        .unwrap();
    session.keyboard().press("q").unwrap();
    session
        .expect_process()
        .timeout(Duration::from_secs(5))
        .to_have_exited_with_code(0)
        .unwrap();
}

#[test]
#[ignore = "requires Go and the isolated Bubble Tea fixture; CI sets TTRY_BUBBLETEA_BIN"]
fn bubbletea_smoke() {
    let binary = required_env("TTRY_BUBBLETEA_BIN");
    let mut launch = LaunchOptions::new(binary).size(50, 10);
    // Bubble Tea 1.x probes xterm colors synchronously during package init.
    // A screen-compatible TERM skips unsupported OSC queries while retaining
    // normal ANSI rendering for this framework-neutral PTY smoke test.
    launch.term = "screen-256color".into();
    let session = TuiSession::launch(launch).unwrap();
    session
        .wait_for_text("Bubble Tea async ready", Duration::from_secs(5))
        .unwrap();
    session.keyboard().press("x").unwrap();
    session
        .wait_for_text("Bubble Tea key: x", Duration::from_secs(5))
        .unwrap();
    session.keyboard().press("q").unwrap();
    session
        .expect_process()
        .timeout(Duration::from_secs(5))
        .to_have_exited_with_code(0)
        .unwrap();
}

#[test]
#[ignore = "requires Python with Textual; CI sets TTRY_TEXTUAL_PYTHON"]
fn textual_smoke() {
    let python = required_env("TTRY_TEXTUAL_PYTHON");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/textual/app.py");
    let session = TuiSession::launch(LaunchOptions::new(python).arg(script).size(50, 10)).unwrap();
    session
        .wait_for_text("Textual async ready", Duration::from_secs(8))
        .unwrap();
    session.keyboard().press("x").unwrap();
    session
        .wait_for_text("Textual key: x", Duration::from_secs(5))
        .unwrap();
    session.keyboard().press("q").unwrap();
    session
        .expect_process()
        .timeout(Duration::from_secs(5))
        .to_have_exited_with_code(0)
        .unwrap();
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is required; leave this test ignored when its toolchain is unavailable")
    })
}

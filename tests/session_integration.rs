use std::time::{Duration, Instant};

use ttry::{expect, LaunchOptions, ProcessState, TuiSession};

fn fixture(mode: &str) -> LaunchOptions {
    LaunchOptions::new(env!("CARGO_BIN_EXE_ttry-fixture"))
        .arg(mode)
        .size(40, 8)
}

#[test]
fn launches_in_pty_with_term_dimensions_and_exit() {
    let session = TuiSession::launch(fixture("info")).unwrap();
    session
        .wait_for_text("TERM=xterm-256color SIZE=40x8", Duration::from_secs(2))
        .unwrap();
    expect(session.process().clone())
        .timeout(Duration::from_secs(2))
        .to_have_exited_with_code(0)
        .unwrap();
}

#[test]
fn relative_executable_resolves_once_against_relative_cwd() {
    let session = TuiSession::launch(
        LaunchOptions::new("./ttry-fixture")
            .arg("info")
            .cwd("target/debug")
            .size(40, 8),
    )
    .unwrap();
    session
        .wait_for_text("TERM=xterm-256color SIZE=40x8", Duration::from_secs(2))
        .unwrap();
}

#[test]
fn zero_startup_timeout_is_enforced() {
    let mut options = fixture("hang");
    options.startup_timeout = Duration::ZERO;
    assert!(matches!(
        TuiSession::launch(options),
        Err(ttry::Error::Timeout { .. })
    ));
}

#[test]
fn keyboard_input_reaches_child_and_updates_screen() {
    let session = TuiSession::launch(fixture("echo")).unwrap();
    session
        .wait_for_text("READY", Duration::from_secs(2))
        .unwrap();
    session.keyboard().paste("hello\n").unwrap();
    session
        .wait_for_text("INPUT:hello", Duration::from_secs(2))
        .unwrap();
    session.keyboard().press("ctrl+c").unwrap();
    session.close().unwrap();
}

#[test]
fn delayed_output_uses_event_driven_assertion() {
    let session = TuiSession::launch(fixture("delayed")).unwrap();
    session
        .expect(session.get_by_text("ready"))
        .timeout(Duration::from_secs(2))
        .to_be_visible()
        .unwrap();
}

#[test]
fn resize_updates_pty_and_screen() {
    let session = TuiSession::launch(fixture("resize")).unwrap();
    session
        .wait_for_text("SIZE:40x8", Duration::from_secs(2))
        .unwrap();
    session.resize(60, 12).unwrap();
    assert_eq!(session.screen().dimensions(), (60, 12));
    session.keyboard().paste("x\n").unwrap();
    session
        .wait_for_text("RESIZED:60x12", Duration::from_secs(2))
        .unwrap();
}

#[test]
fn cleanup_is_bounded_idempotent_and_leaves_no_child() {
    let mut options = fixture("hang");
    options.shutdown_timeout = Duration::from_millis(300);
    let session = TuiSession::launch(options).unwrap();
    let started = Instant::now();
    session.close().unwrap();
    session.close().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(session.process().state(), ProcessState::Exited(_)));
}

#[cfg(unix)]
#[test]
fn drop_cleanup_reaps_the_child_process() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let pid = {
        let mut options = fixture("hang");
        options.shutdown_timeout = Duration::from_millis(300);
        let session = TuiSession::launch(options).unwrap();
        session.process().process_id().unwrap()
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while kill(Pid::from_raw(pid as i32), None).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(kill(Pid::from_raw(pid as i32), None).is_err());
}

#[cfg(unix)]
#[test]
fn cleanup_terminates_the_entire_process_group() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let mut options = fixture("tree");
    options.shutdown_timeout = Duration::from_millis(900);
    let session = TuiSession::launch(options).unwrap();
    session
        .wait_for_text("DESCENDANT_PID=", Duration::from_secs(2))
        .unwrap();
    let descendant = session
        .screen()
        .text()
        .split("DESCENDANT_PID=")
        .nth(1)
        .and_then(|suffix| suffix.split_whitespace().next())
        .and_then(|value| value.parse::<i32>().ok())
        .expect("fixture should print its descendant PID");
    session.close().unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while kill(Pid::from_raw(descendant), None).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(kill(Pid::from_raw(descendant), None).is_err());
}

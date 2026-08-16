#![cfg(feature = "internal-test-fixture")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ttry::{expect, LaunchOptions, ProcessState, TuiSession};

const OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);

fn fixture(mode: &str) -> LaunchOptions {
    LaunchOptions::new(env!("CARGO_BIN_EXE_ttry-fixture"))
        .arg(mode)
        .size(40, 8)
}

#[cfg(unix)]
fn descendant_pid(session: &TuiSession, prefix: &str) -> i32 {
    let locator = session
        .screen()
        .get_by_regex(&format!(r"{prefix}[0-9]+"))
        .unwrap();
    session
        .expect(locator.clone())
        .timeout(OUTPUT_TIMEOUT)
        .to_be_visible()
        .unwrap();
    locator
        .text()
        .unwrap()
        .strip_prefix(prefix)
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn launches_in_pty_with_term_dimensions_and_exit() {
    let session = TuiSession::launch(fixture("info")).unwrap();
    #[cfg(unix)]
    let pid = session.process().process_id().unwrap() as i32;
    session
        .wait_for_text("TERM=xterm-256color SIZE=40x8", OUTPUT_TIMEOUT)
        .unwrap();
    expect(session.process().clone())
        .timeout(OUTPUT_TIMEOUT)
        .to_have_exited_with_code(0)
        .unwrap();
    #[cfg(unix)]
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
        "terminal state observation should reap a child with no live descendants"
    );
}

#[test]
fn relative_executable_resolves_once_against_relative_cwd() {
    let fixture_binary = PathBuf::from(env!("CARGO_BIN_EXE_ttry-fixture"));
    let fixture_directory = fixture_binary
        .parent()
        .expect("Cargo fixture binary should have a parent directory");
    let relative_fixture = PathBuf::from(".").join(
        fixture_binary
            .file_name()
            .expect("Cargo fixture binary should have a filename"),
    );
    let session = TuiSession::launch(
        LaunchOptions::new(relative_fixture)
            .arg("info")
            .cwd(fixture_directory)
            .size(40, 8),
    )
    .unwrap();
    session
        .wait_for_text("TERM=xterm-256color SIZE=40x8", OUTPUT_TIMEOUT)
        .unwrap();
}

#[test]
fn zero_startup_timeout_is_rejected_as_invalid_input() {
    let mut options = fixture("hang");
    options.startup_timeout = Duration::ZERO;
    assert!(matches!(
        TuiSession::launch(options),
        Err(ttry::Error::InvalidTimeout {
            field: "startup_timeout"
        })
    ));
}

#[test]
fn excessive_startup_timeout_is_rejected_as_invalid_input() {
    let mut options = fixture("hang");
    options.startup_timeout = Duration::MAX;
    assert!(matches!(
        TuiSession::launch(options),
        Err(ttry::Error::InvalidTimeout {
            field: "startup_timeout"
        })
    ));
}

#[test]
fn zero_shutdown_timeout_is_rejected_as_invalid_input() {
    let mut options = fixture("hang");
    options.shutdown_timeout = Duration::ZERO;
    assert!(matches!(
        TuiSession::launch(options),
        Err(ttry::Error::InvalidTimeout {
            field: "shutdown_timeout"
        })
    ));
}

#[test]
fn zero_operation_timeouts_are_rejected_as_invalid_input() {
    let session = TuiSession::launch(fixture("hang")).unwrap();

    assert!(matches!(
        session.wait_for_text("anything", Duration::ZERO),
        Err(ttry::Error::InvalidTimeout { field: "timeout" })
    ));
    assert!(matches!(
        expect(session.process().clone())
            .timeout(Duration::ZERO)
            .to_be_running(),
        Err(ttry::Error::InvalidTimeout { field: "timeout" })
    ));
    assert!(matches!(
        session.wait_for_text("anything", Duration::MAX),
        Err(ttry::Error::InvalidTimeout { field: "timeout" })
    ));
    assert!(matches!(
        expect(session.process().clone())
            .timeout(Duration::MAX)
            .to_have_exited(),
        Err(ttry::Error::InvalidTimeout { field: "timeout" })
    ));

    session.close().unwrap();
}

#[test]
fn expired_startup_budget_cleans_up_the_completed_spawn() {
    let mut options = fixture("hang");
    options.startup_timeout = Duration::from_nanos(1);
    options.shutdown_timeout = Duration::from_millis(300);
    let started = Instant::now();
    assert!(matches!(
        TuiSession::launch(options),
        Err(ttry::Error::Timeout { .. })
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn launch_error_is_not_masked_by_an_expired_startup_budget() {
    let mut options = LaunchOptions::new("ttry-command-that-does-not-exist");
    options.startup_timeout = Duration::from_nanos(1);

    assert!(matches!(
        TuiSession::launch(options),
        Err(ttry::Error::Launch { .. })
    ));
}

#[test]
fn keyboard_input_reaches_child_and_updates_screen() {
    let session = TuiSession::launch(fixture("echo")).unwrap();
    session.wait_for_text("READY", OUTPUT_TIMEOUT).unwrap();
    session.keyboard().paste("hello\n").unwrap();
    session
        .wait_for_text("INPUT:hello", OUTPUT_TIMEOUT)
        .unwrap();
    session.keyboard().press("ctrl+c").unwrap();
    session.close().unwrap();
}

#[test]
fn delayed_output_uses_event_driven_assertion() {
    let session = TuiSession::launch(fixture("delayed")).unwrap();
    session
        .expect(session.get_by_text("ready"))
        .timeout(OUTPUT_TIMEOUT)
        .to_be_visible()
        .unwrap();
}

#[test]
fn resize_updates_pty_and_screen() {
    let session = TuiSession::launch(fixture("resize")).unwrap();
    session.wait_for_text("SIZE:40x8", OUTPUT_TIMEOUT).unwrap();
    assert!(matches!(
        session.resize(0, 12),
        Err(ttry::Error::InvalidDimensions { .. })
    ));
    assert_eq!(session.screen().dimensions(), (40, 8));
    session.resize(60, 12).unwrap();
    assert_eq!(session.screen().dimensions(), (60, 12));
    session.keyboard().paste("x\n").unwrap();
    session
        .wait_for_text("RESIZED:60x12", OUTPUT_TIMEOUT)
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
    assert!(session
        .process()
        .recent_events()
        .iter()
        .any(|event| event == "PTY reader joined"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        session.process().state().unwrap(),
        ProcessState::Exited(_)
    ));
}

#[cfg(unix)]
#[test]
fn concurrent_state_polling_and_close_never_observe_a_reap_race() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    for _ in 0..20 {
        let mut options = fixture("hang");
        options.shutdown_timeout = Duration::from_millis(90);
        let session = TuiSession::launch(options).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let process = session.process().clone();
        let stop = Arc::new(AtomicBool::new(false));
        let poll_stop = Arc::clone(&stop);
        let poller = std::thread::spawn(move || {
            while !poll_stop.load(Ordering::Acquire) {
                match process.state() {
                    Ok(ProcessState::Running) => std::thread::yield_now(),
                    Ok(ProcessState::Exited(_)) => return None,
                    Err(error) => {
                        return Some(format!("{error}; events={:?}", process.recent_events()));
                    }
                }
            }
            None
        });

        session.close().unwrap();
        stop.store(true, Ordering::Release);
        assert_eq!(poller.join().unwrap(), None);
    }
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
    assert_eq!(
        session.process().process_group_id(),
        session.process().process_id().map(|pid| pid as i32)
    );
    let descendant = descendant_pid(&session, "DESCENDANT_PID=");
    assert_eq!(
        nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(descendant))).unwrap(),
        nix::unistd::Pid::from_raw(session.process().process_group_id().unwrap())
    );
    session.close().unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while kill(Pid::from_raw(descendant), None).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(kill(Pid::from_raw(descendant), None).is_err());
}

#[cfg(unix)]
#[test]
fn exited_leader_preserves_the_descendant_grace_period() {
    let temp = tempfile::tempdir().unwrap();
    let completion_marker = temp.path().join("descendant-completion");
    let mut options = fixture("grace-tree");
    // Four seconds exceeds the complete three-second pre-close observation
    // budget below. A fifteen-second shutdown still gives it a five-second
    // grace phase in which to finish naturally.
    options.shutdown_timeout = Duration::from_secs(15);
    options = options
        .env("TTRY_SIGNAL_MARKER", completion_marker.as_os_str())
        .env("TTRY_GRACE_DELAY", "4");
    let session = TuiSession::launch(options).unwrap();
    let descendant = descendant_pid(&session, "DESCENDANT_PID=");
    assert_eq!(
        nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(descendant))).unwrap(),
        nix::unistd::Pid::from_raw(session.process().process_group_id().unwrap())
    );
    expect(session.process().clone())
        .timeout(Duration::from_secs(1))
        .to_have_exited_with_code(0)
        .unwrap();
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(descendant), None).is_ok());

    session.close().unwrap();
    let completion = std::fs::read_to_string(&completion_marker).unwrap_or_else(|error| {
        panic!(
            "missing descendant completion marker: {error}; alive={}; events={:?}",
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(descendant), None).is_ok(),
            session.process().recent_events()
        )
    });
    assert_eq!(completion, "natural");
}

#[cfg(unix)]
#[test]
fn cached_exit_is_rechecked_and_reaped_after_descendants_finish() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let temp = tempfile::tempdir().unwrap();
    let completion_marker = temp.path().join("cached-reap-completion");
    let mut options = fixture("grace-tree");
    options.shutdown_timeout = Duration::from_secs(3);
    options = options
        .env("TTRY_SIGNAL_MARKER", completion_marker.as_os_str())
        .env("TTRY_GRACE_DELAY", "0.6");
    let session = TuiSession::launch(options).unwrap();
    session
        .wait_for_text("DESCENDANT_PID=", OUTPUT_TIMEOUT)
        .unwrap();
    let leader = Pid::from_raw(session.process().process_id().unwrap() as i32);
    expect(session.process().clone())
        .timeout(Duration::from_secs(1))
        .to_have_exited_with_code(0)
        .unwrap();
    assert!(
        kill(leader, None).is_ok(),
        "the exited leader must reserve its PGID while a descendant is live"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    while !completion_marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(completion_marker).unwrap(),
        "natural"
    );
    assert!(matches!(
        session.process().state().unwrap(),
        ProcessState::Exited(_)
    ));
    assert!(
        kill(leader, None).is_err(),
        "a later state query must reap the cached exit once the group is empty"
    );
}

#[cfg(unix)]
#[test]
fn exited_leader_still_sends_term_to_live_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("term-cleanup");
    let mut options = fixture("term-tree");
    options.shutdown_timeout = Duration::from_millis(900);
    options = options.env("TTRY_SIGNAL_MARKER", marker.as_os_str());
    let session = TuiSession::launch(options).unwrap();
    session
        .wait_for_text("TERM_DESCENDANT_PID=", OUTPUT_TIMEOUT)
        .unwrap();
    expect(session.process().clone())
        .timeout(Duration::from_secs(1))
        .to_have_exited_with_code(0)
        .unwrap();

    session.close().unwrap();
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "term");
}

#[cfg(unix)]
#[test]
fn exited_leader_still_offers_eof_to_interactive_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("eof-cleanup");
    let mut options = fixture("eof-tree");
    options.shutdown_timeout = Duration::from_millis(900);
    options = options.env("TTRY_SIGNAL_MARKER", marker.as_os_str());
    let session = TuiSession::launch(options).unwrap();
    session
        .wait_for_text("EOF_DESCENDANT_PID=", OUTPUT_TIMEOUT)
        .unwrap();
    expect(session.process().clone())
        .timeout(Duration::from_secs(1))
        .to_have_exited_with_code(0)
        .unwrap();

    session.close().unwrap();
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "eof");
}

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::error::validate_timeout;
use crate::process::ProcessState;
use crate::screen::validate_dimensions;
use crate::session::{LaunchOptions, TuiSession};
use crate::{Error, Result};

pub const MAX_TIMEOUT_MS: u64 = 86_400_000;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Reporter {
    #[default]
    List,
    Dot,
}

impl std::str::FromStr for Reporter {
    type Err = String;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "list" => Ok(Self::List),
            "dot" => Ok(Self::Dot),
            _ => Err(format!(
                "unknown reporter `{value}`; expected `list` or `dot`"
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub timeout_ms: u64,
    pub reporter: Reporter,
    pub tests: Vec<ConfiguredTest>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout_ms: 5_000,
            reporter: Reporter::List,
            tests: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|error| Error::Config {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let mut config: Self = toml::from_str(&source).map_err(|error| Error::Config {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|directory| directory.join(path))
                .map_err(|error| Error::Config {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                })?
        };
        let config_directory = absolute_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if !(1..=MAX_TIMEOUT_MS).contains(&config.timeout_ms) {
            return Err(Error::Config {
                path: path.to_path_buf(),
                message: format!(
                    "timeout_ms must be greater than zero and at most {MAX_TIMEOUT_MS}"
                ),
            });
        }
        for test in &mut config.tests {
            if test.name.trim().is_empty() || test.command.trim().is_empty() {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: "every [[tests]] entry requires non-empty name and command".into(),
                });
            }
            if test
                .timeout_ms
                .is_some_and(|value| !(1..=MAX_TIMEOUT_MS).contains(&value))
            {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!(
                        "timeout_ms for test `{}` must be greater than zero and at most {MAX_TIMEOUT_MS}",
                        test.name
                    ),
                });
            }
            if let Err(error) =
                validate_dimensions(test.cols.unwrap_or(80), test.rows.unwrap_or(24))
            {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!("invalid dimensions for test `{}`: {error}", test.name),
                });
            }
            if test.expect_text.as_deref() == Some("") {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!("expect_text for test `{}` must not be empty", test.name),
                });
            }
            if test.allow_running && (test.expect_exit || test.expect_exit_code.is_some()) {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!(
                        "allow_running cannot be combined with expect_exit or expect_exit_code for test `{}`",
                        test.name
                    ),
                });
            }
            if test
                .allow_running_grace_ms
                .is_some_and(|value| !(1..=MAX_TIMEOUT_MS).contains(&value))
            {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!(
                        "allow_running_grace_ms for test `{}` must be greater than zero and at most {MAX_TIMEOUT_MS}",
                        test.name
                    ),
                });
            }
            if test
                .shutdown_timeout_ms
                .is_some_and(|value| !(1..=MAX_TIMEOUT_MS).contains(&value))
            {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!(
                        "shutdown_timeout_ms for test `{}` must be greater than zero and at most {MAX_TIMEOUT_MS}",
                        test.name
                    ),
                });
            }
            test.cwd = Some(match test.cwd.take() {
                Some(cwd) if cwd.is_absolute() => cwd,
                Some(cwd) => config_directory.join(cwd),
                None => config_directory.clone(),
            });
        }
        Ok(config)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfiguredTest {
    pub name: String,
    pub group: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub input: Option<String>,
    pub expect_text: Option<String>,
    /// Opt out of the default successful-exit assertion after screen checks.
    /// Intended for interactive TUIs that must remain alive until test
    /// cleanup. Any exit observed during the bounded grace window is rejected.
    #[serde(default)]
    pub allow_running: bool,
    /// How long an `allow_running` test must remain alive after its final
    /// assertion. Defaults to 50 ms and can be increased for applications
    /// whose asynchronous startup work can fail later.
    pub allow_running_grace_ms: Option<u64>,
    /// Wait for any process exit. `expect_exit_code` implies this and also
    /// checks the exact code.
    pub expect_exit: bool,
    pub expect_exit_code: Option<i32>,
    pub skip: bool,
    pub focus: bool,
    pub timeout_ms: Option<u64>,
    /// Bounded process-tree and PTY-reader cleanup timeout. Defaults to 600 ms.
    pub shutdown_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestStatus {
    Passed,
    Failed(String),
    Skipped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestResult {
    pub name: String,
    pub status: TestStatus,
    pub duration: Duration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunReport {
    pub reporter: Reporter,
    pub selected: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub tests: Vec<TestResult>,
}

impl RunReport {
    pub fn success(&self) -> bool {
        self.selected > 0 && self.failed == 0
    }
    pub fn summary(&self) -> String {
        format!(
            "{} passed, {} failed, {} skipped",
            self.passed, self.failed, self.skipped
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    pub grep: Option<String>,
    pub timeout: Option<Duration>,
    pub reporter: Option<Reporter>,
    pub update_snapshots: bool,
}
pub struct TestContext {
    sessions: Arc<Mutex<Vec<TuiSession>>>,
    cancelled: Arc<AtomicBool>,
    update_snapshots: bool,
}

impl TestContext {
    pub fn tui(&mut self, options: LaunchOptions) -> Result<TuiSession> {
        if self.is_cancelled() {
            return Err(Error::Runner(
                "test was cancelled before launching a new TUI session".into(),
            ));
        }
        let session = TuiSession::launch(options)?;
        let mut sessions = self
            .sessions
            .lock()
            .expect("session registry lock poisoned");
        if self.is_cancelled() {
            drop(sessions);
            let _ = session.close();
            return Err(Error::Runner(
                "test was cancelled while launching a TUI session".into(),
            ));
        }
        sessions.push(session.clone());
        Ok(session)
    }
    /// Returns true once the runner has reached this test's timeout.
    ///
    /// Rust test bodies cannot be forcibly stopped safely. Long-running custom
    /// work should poll this flag and return promptly when cancellation is
    /// requested. Registered TUI sessions are closed during cancellation; if
    /// the worker does not stop within the bounded grace period, later tests
    /// are skipped rather than run concurrently with it.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    /// Returns snapshot settings derived from the runner's typed options.
    pub fn snapshot_options(&self, preserve_width: bool) -> crate::SnapshotOptions {
        crate::SnapshotOptions {
            update: self.update_snapshots,
            preserve_width,
        }
    }
}

fn cleanup_sessions(sessions: &Mutex<Vec<TuiSession>>) -> Result<()> {
    let mut first_error = None;
    let sessions: Vec<_> = sessions
        .lock()
        .expect("session registry lock poisoned")
        .drain(..)
        .collect();
    for session in sessions.into_iter().rev() {
        if let Err(error) = session.close() {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn combine_test_and_cleanup(body: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (body, cleanup) {
        (Ok(()), cleanup) => cleanup,
        (Err(body), Ok(())) => Err(body),
        (Err(body), Err(cleanup)) => Err(Error::Runner(format!(
            "{body}; cleanup also failed: {cleanup}"
        ))),
    }
}

type TestBody = Box<dyn FnOnce(&mut TestContext) -> Result<()> + Send>;
const WORKER_CANCELLATION_GRACE: Duration = Duration::from_millis(100);
const ALLOW_RUNNING_EXIT_GRACE: Duration = Duration::from_millis(50);

pub struct TestCase {
    name: String,
    group: Option<String>,
    skip: bool,
    focus: bool,
    timeout: Option<Duration>,
    body: TestBody,
}

impl TestCase {
    pub fn new(
        name: impl Into<String>,
        body: impl FnOnce(&mut TestContext) -> Result<()> + Send + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            group: None,
            skip: false,
            focus: false,
            timeout: None,
            body: Box::new(body),
        }
    }
    pub fn describe(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }
    pub fn skip(mut self) -> Self {
        self.skip = true;
        self
    }
    pub fn focus(mut self) -> Self {
        self.focus = true;
        self
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
    fn full_name(&self) -> String {
        self.group.as_ref().map_or_else(
            || self.name.clone(),
            |group| format!("{group} › {}", self.name),
        )
    }
}

pub struct Runner {
    tests: Vec<TestCase>,
    timeout: Duration,
    update_snapshots: bool,
}

impl Runner {
    pub fn new(timeout: Duration) -> Self {
        Self {
            tests: Vec::new(),
            timeout,
            update_snapshots: false,
        }
    }
    pub fn update_snapshots(mut self, update: bool) -> Self {
        self.update_snapshots = update;
        self
    }
    pub fn register(&mut self, test: TestCase) {
        self.tests.push(test);
    }
    pub fn run(self, grep: Option<&str>) -> RunReport {
        let focused = self.tests.iter().any(|test| test.focus);
        let mut report = RunReport::default();
        let mut unsafe_to_continue = false;
        for (test_index, test) in self.tests.into_iter().enumerate() {
            let name = test.full_name();
            let filtered = grep.is_some_and(|pattern| !name.contains(pattern));
            if !filtered && (!focused || test.focus) {
                report.selected += 1;
            }
            if unsafe_to_continue || test.skip || filtered || (focused && !test.focus) {
                report.skipped += 1;
                report.tests.push(TestResult {
                    name,
                    status: TestStatus::Skipped,
                    duration: Duration::ZERO,
                });
                continue;
            }
            let timeout = test.timeout.unwrap_or(self.timeout);
            if let Err(error) = validate_timeout(timeout, "timeout") {
                report.failed += 1;
                report.tests.push(TestResult {
                    name,
                    status: TestStatus::Failed(error.to_string()),
                    duration: Duration::ZERO,
                });
                continue;
            }
            let started = Instant::now();
            let sessions = Arc::new(Mutex::new(Vec::new()));
            let cancelled = Arc::new(AtomicBool::new(false));
            let worker_sessions = Arc::clone(&sessions);
            let worker_cancelled = Arc::clone(&cancelled);
            let update_snapshots = self.update_snapshots;
            let (result_sender, result_receiver) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name(format!("ttry-test-{test_index}"))
                .spawn(move || {
                    let mut context = TestContext {
                        sessions: worker_sessions,
                        cancelled: worker_cancelled,
                        update_snapshots,
                    };
                    let result = catch_unwind(AssertUnwindSafe(|| (test.body)(&mut context)));
                    let result = match result {
                        Ok(result) => result,
                        Err(payload) => {
                            let message = payload
                                .downcast_ref::<&str>()
                                .copied()
                                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                                .unwrap_or("unknown panic payload");
                            Err(Error::Runner(format!("test body panicked: {message}")))
                        }
                    };
                    let _ = result_sender.send(result);
                });
            let (result, can_continue) = match worker {
                Ok(worker) => match result_receiver.recv_timeout(timeout) {
                    Ok(body_result) => {
                        let cleanup_result = cleanup_sessions(&sessions);
                        let _ = worker.join();
                        (combine_test_and_cleanup(body_result, cleanup_result), true)
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        cancelled.store(true, Ordering::Release);
                        let cleanup_result = cleanup_sessions(&sessions);
                        let mut diagnostic = cleanup_result
                            .err()
                            .map(|error| format!("; after cancellation: {error}"))
                            .unwrap_or_default();
                        let worker_stopped =
                            match result_receiver.recv_timeout(WORKER_CANCELLATION_GRACE) {
                                Ok(body_result) => {
                                    match body_result {
                                    Ok(()) => diagnostic.push_str(
                                        "; worker stopped cooperatively after timeout cancellation",
                                    ),
                                    Err(error) => diagnostic.push_str(&format!(
                                        "; worker stopped after cancellation: {error}"
                                    )),
                                }
                                    let _ = worker.join();
                                    true
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => {
                                    let _ = worker.join();
                                    true
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => false,
                            };
                        if !worker_stopped {
                            diagnostic.push_str(
                                "; worker ignored cancellation; remaining tests were skipped",
                            );
                        }
                        (
                            Err(Error::Timeout {
                                timeout,
                                context: format!("test `{name}` exceeded its timeout{diagnostic}"),
                            }),
                            worker_stopped,
                        )
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let cleanup_result = cleanup_sessions(&sessions);
                        let _ = worker.join();
                        (
                            combine_test_and_cleanup(
                                Err(Error::Runner(format!(
                                    "worker for test `{name}` stopped unexpectedly"
                                ))),
                                cleanup_result,
                            ),
                            true,
                        )
                    }
                },
                Err(error) => (
                    Err(Error::Runner(format!(
                        "could not start worker for test `{name}`: {error}"
                    ))),
                    true,
                ),
            };
            unsafe_to_continue = !can_continue;
            let duration = started.elapsed();
            match result {
                Ok(()) => {
                    report.passed += 1;
                    report.tests.push(TestResult {
                        name,
                        status: TestStatus::Passed,
                        duration,
                    });
                }
                Err(error) => {
                    report.failed += 1;
                    report.tests.push(TestResult {
                        name,
                        status: TestStatus::Failed(error.to_string()),
                        duration,
                    });
                }
            }
        }
        report
    }
}

pub fn run_config(config: Config, options: RunOptions) -> RunReport {
    let selected_reporter = options
        .reporter
        .clone()
        .unwrap_or_else(|| config.reporter.clone());
    let cli_timeout = options.timeout;
    if let Some(cli_timeout) = cli_timeout {
        if let Err(error) = validate_timeout(cli_timeout, "timeout") {
            return RunReport {
                reporter: selected_reporter,
                selected: 1,
                failed: 1,
                tests: vec![TestResult {
                    name: "configuration".into(),
                    status: TestStatus::Failed(error.to_string()),
                    duration: Duration::ZERO,
                }],
                ..RunReport::default()
            };
        }
    }
    let timeout = cli_timeout.unwrap_or(Duration::from_millis(config.timeout_ms));
    let mut runner = Runner::new(timeout).update_snapshots(options.update_snapshots);
    for configured in config.tests {
        let name = configured.name.clone();
        // Precedence is per-test config, then CLI suite override, then the
        // configured suite default.
        let test_timeout = configured
            .timeout_ms
            .map(Duration::from_millis)
            .or(cli_timeout)
            .unwrap_or(timeout);
        let group = configured.group.clone();
        let skip = configured.skip;
        let focus = configured.focus;
        let timeout_context = format!("running configured test `{}`", configured.name);
        let mut test = TestCase::new(name, move |context| {
            let deadline = Instant::now() + test_timeout;
            let remaining = || {
                let duration = deadline.saturating_duration_since(Instant::now());
                if duration.is_zero() {
                    Err(Error::Timeout {
                        timeout: test_timeout,
                        context: timeout_context.clone(),
                    })
                } else {
                    Ok(duration)
                }
            };
            let mut launch = LaunchOptions::new(configured.command)
                .args(configured.args)
                .size(configured.cols.unwrap_or(80), configured.rows.unwrap_or(24));
            launch.startup_timeout = remaining()?;
            launch.shutdown_timeout =
                Duration::from_millis(configured.shutdown_timeout_ms.unwrap_or(600));
            if let Some(cwd) = configured.cwd {
                launch = launch.cwd(cwd);
            }
            let session = context.tui(launch)?;
            if let Some(input) = configured.input {
                session.keyboard().paste(&input)?;
            }
            if let Some(expected) = configured.expect_text {
                if let Err(error) = session.wait_for_text(&expected, remaining()?) {
                    return match error {
                        Error::ProcessExited(message) if configured.allow_running => {
                            Err(Error::ProcessExited(format!(
                                "{message}; allow_running requires the process to remain alive"
                            )))
                        }
                        error => Err(error),
                    };
                }
            }
            if let Some(code) = configured.expect_exit_code {
                expect_configured_exit(
                    &session,
                    Some(code),
                    deadline,
                    test_timeout,
                    &timeout_context,
                )?;
            } else if configured.expect_exit {
                expect_configured_exit(&session, None, deadline, test_timeout, &timeout_context)?;
            } else if !configured.allow_running {
                expect_configured_exit(
                    &session,
                    Some(0),
                    deadline,
                    test_timeout,
                    &timeout_context,
                )?;
            } else {
                // `allow_running` requires an interactive process to remain
                // alive through a short bounded grace window so an immediate
                // post-assertion exit cannot race a single sample.
                let configured_grace = configured
                    .allow_running_grace_ms
                    .map(Duration::from_millis)
                    .unwrap_or(ALLOW_RUNNING_EXIT_GRACE);
                let exit_deadline = Instant::now() + configured_grace.min(remaining()?);
                loop {
                    if let ProcessState::Exited(status) = session.process().state()? {
                        return Err(Error::ProcessExited(format!(
                            "{status}; allow_running requires the process to remain alive"
                        )));
                    }
                    let remaining = exit_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1).min(remaining));
                }
            }
            Ok(())
        });
        test = test.timeout(test_timeout);
        if let Some(group) = group {
            test = test.describe(group);
        }
        if skip {
            test = test.skip();
        }
        if focus {
            test = test.focus();
        }
        runner.register(test);
    }
    let mut report = runner.run(options.grep.as_deref());
    report.reporter = selected_reporter;
    report
}

fn expect_configured_exit(
    session: &TuiSession,
    expected_code: Option<i32>,
    deadline: Instant,
    timeout: Duration,
    timeout_context: &str,
) -> Result<()> {
    // Always take one nonblocking sample before rejecting an exhausted test
    // budget. A preceding output assertion may consume the final instant even
    // though the child has already exited successfully.
    if let ProcessState::Exited(status) = session.process().state()? {
        if expected_code.is_none() || status.code == expected_code {
            return Ok(());
        }
        return Err(Error::ProcessExited(format!(
            "expected exit code {}, got {status}",
            expected_code.expect("checked above")
        )));
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::Timeout {
            timeout,
            context: timeout_context.into(),
        });
    }
    let expectation = session.expect_process().timeout(remaining);
    if let Some(code) = expected_code {
        expectation.to_have_exited_with_code(code)
    } else {
        expectation.to_have_exited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_focus_skip_and_cleanup_are_deterministic() {
        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(TestCase::new("ignored", |_| Ok(())).skip());
        runner.register(
            TestCase::new("focused", |_| Ok(()))
                .describe("group")
                .focus(),
        );
        runner.register(TestCase::new("not focused", |_| Ok(())));
        let report = runner.run(None);
        assert_eq!((report.passed, report.failed, report.skipped), (1, 0, 2));
        assert_eq!(report.tests[1].name, "group › focused");
    }
    #[test]
    fn empty_or_unmatched_test_selection_is_not_successful() {
        assert!(!Runner::new(Duration::from_secs(1)).run(None).success());

        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(TestCase::new("present", |_| Ok(())));
        let report = runner.run(Some("missing"));
        assert_eq!(report.selected, 0);
        assert!(!report.success());
    }
    #[test]
    fn an_explicitly_skipped_only_suite_is_successful() {
        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(TestCase::new("not available here", |_| Ok(())).skip());

        let report = runner.run(None);

        assert_eq!((report.selected, report.skipped, report.failed), (1, 1, 0));
        assert!(report.success());
    }
    #[test]
    fn invalid_config_is_readable() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "timeout_ms = 0\nunknown = true").unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { .. })
        ));
    }
    #[test]
    fn invalid_reporter_is_rejected_during_deserialization() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "reporter = 'quiet'\n").unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { .. })
        ));
    }
    #[test]
    fn zero_allow_running_grace_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "[[tests]]\nname = 'interactive'\ncommand = 'app'\nallow_running = true\nallow_running_grace_ms = 0\n",
        )
        .unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { message, .. })
                if message.contains("allow_running_grace_ms")
                    && message.contains("greater than zero")
        ));
    }
    #[test]
    fn zero_configured_shutdown_timeout_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "[[tests]]\nname = 'case'\ncommand = 'app'\nshutdown_timeout_ms = 0\n",
        )
        .unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { message, .. })
                if message.contains("shutdown_timeout_ms")
                    && message.contains("greater than zero")
        ));
    }
    #[test]
    fn run_config_centralizes_reporter_override_resolution() {
        let config = Config {
            reporter: Reporter::Dot,
            ..Config::default()
        };
        assert_eq!(
            run_config(config.clone(), RunOptions::default()).reporter,
            Reporter::Dot
        );
        assert_eq!(
            run_config(
                config,
                RunOptions {
                    reporter: Some(Reporter::List),
                    ..RunOptions::default()
                }
            )
            .reporter,
            Reporter::List
        );
    }
    #[test]
    fn zero_per_test_timeout_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "[[tests]]\nname = 'case'\ncommand = 'true'\ntimeout_ms = 0\n",
        )
        .unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { .. })
        ));
    }
    #[test]
    fn excessive_configured_timeout_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            format!("timeout_ms = {}\n", MAX_TIMEOUT_MS + 1),
        )
        .unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { message, .. }) if message.contains("at most")
        ));
    }
    #[test]
    fn empty_expected_text_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "[[tests]]\nname = 'case'\ncommand = 'true'\nexpect_text = ''\n",
        )
        .unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { .. })
        ));
    }
    #[test]
    fn legacy_config_without_allow_running_uses_the_safe_default() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "[[tests]]\nname = 'legacy'\ncommand = 'true'\n",
        )
        .unwrap();
        let config = Config::load(file.path()).unwrap();
        assert!(!config.tests[0].allow_running);
    }
    #[test]
    fn loaded_config_resolves_cwd_without_rewriting_commands() {
        let directory = tempfile::tempdir().unwrap();
        let config_directory = directory.path().join("suite");
        fs::create_dir(&config_directory).unwrap();
        let path = config_directory.join("ttry.toml");
        fs::write(
            &path,
            "[[tests]]\nname = 'default cwd'\ncommand = 'true'\n\
             [[tests]]\nname = 'relative cwd'\ncommand = './tool'\ncwd = 'fixtures'\n",
        )
        .unwrap();

        let config = Config::load(path).unwrap();

        assert_eq!(
            config.tests[0].cwd.as_deref(),
            Some(config_directory.as_path())
        );
        let relative_directory = config_directory.join("fixtures");
        assert_eq!(
            config.tests[1].cwd.as_deref(),
            Some(relative_directory.as_path())
        );
        assert_eq!(config.tests[1].command, "./tool");
    }
    #[test]
    fn zero_configured_dimensions_are_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "[[tests]]\nname = 'case'\ncommand = 'true'\ncols = 0\nrows = 24\n",
        )
        .unwrap();
        assert!(matches!(
            Config::load(file.path()),
            Err(Error::Config { .. })
        ));
    }
    #[cfg(unix)]
    #[test]
    fn configured_dimensions_reach_the_child_environment() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "custom size".into(),
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'SIZE=%sx%s\\r\\n' \"$COLUMNS\" \"$LINES\"".into(),
                ],
                cols: Some(100),
                rows: Some(30),
                expect_text: Some("SIZE=100x30".into()),
                expect_exit_code: Some(0),
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (1, 0), "{report:?}");
    }
    #[cfg(unix)]
    #[test]
    fn configured_test_can_explicitly_require_exit_without_a_code() {
        let config = Config {
            timeout_ms: 40,
            tests: vec![ConfiguredTest {
                name: "hang".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 1".into()],
                expect_exit: true,
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (0, 1));
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("timed out")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn configured_text_check_can_pass_while_tui_remains_running() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "long-running tui".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "printf 'READY\\r\\n'; sleep 2".into()],
                expect_text: Some("READY".into()),
                allow_running: true,
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (1, 0), "{report:?}");
    }
    #[cfg(unix)]
    #[test]
    fn allow_running_rejects_a_nonzero_exit_observed_after_the_text_check() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "crashed interactive tui".into(),
                command: "/bin/sh".into(),
                // The leader exits before its descendant emits the expected
                // text, making the failed status deterministic at the
                // allow_running state check.
                args: vec![
                    "-c".into(),
                    "(sleep 0.05; printf 'READY\\r\\n') & exit 7".into(),
                ],
                expect_text: Some("READY".into()),
                allow_running: true,
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (0, 1), "{report:?}");
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("exit code 7")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn allow_running_rejects_a_clean_exit_after_the_text_check() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "early clean exit".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "printf 'READY\r\n'; exit 0".into()],
                expect_text: Some("READY".into()),
                allow_running: true,
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (0, 1), "{report:?}");
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message)
                if message.contains("exit code 0") && message.contains("remain alive")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn allow_running_catches_a_crash_just_after_the_text_check() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "slightly delayed crash".into(),
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'READY\\r\\n'; sleep 0.02; exit 9".into(),
                ],
                expect_text: Some("READY".into()),
                allow_running: true,
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (0, 1), "{report:?}");
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("exit code 9")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn configurable_allow_running_grace_catches_a_later_crash() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "later asynchronous crash".into(),
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf 'READY\\r\\n'; sleep 0.08; exit 11".into(),
                ],
                expect_text: Some("READY".into()),
                allow_running: true,
                allow_running_grace_ms: Some(500),
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (0, 1), "{report:?}");
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("exit code 11")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn configured_text_check_rejects_a_later_nonzero_exit_by_default() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "crashing command".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "printf 'READY\\r\\n'; exit 7".into()],
                expect_text: Some("READY".into()),
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (0, 1), "{report:?}");
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("exit code 7")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn configured_exit_samples_an_exited_process_after_the_deadline() {
        let session =
            TuiSession::launch(LaunchOptions::new("/bin/sh").args(["-c", "exit 0"])).unwrap();
        let observation_deadline = Instant::now() + Duration::from_secs(1);
        while !matches!(session.process().state().unwrap(), ProcessState::Exited(_)) {
            assert!(Instant::now() < observation_deadline);
            thread::yield_now();
        }

        let result = expect_configured_exit(
            &session,
            Some(0),
            Instant::now(),
            Duration::from_millis(10),
            "regression test",
        );

        assert!(result.is_ok(), "{result:?}");
    }
    #[test]
    fn snapshot_update_mode_is_available_through_test_context() {
        let observed = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&observed);
        let mut runner = Runner::new(Duration::from_secs(1)).update_snapshots(true);
        runner.register(TestCase::new("snapshot options", move |context| {
            marker.store(context.snapshot_options(false).update, Ordering::Release);
            Ok(())
        }));
        assert!(runner.run(None).success());
        assert!(observed.load(Ordering::Acquire));
    }
    #[test]
    fn non_cooperative_timeout_skips_later_tests() {
        let reached_next_test = Arc::new(AtomicBool::new(false));
        let next_test_marker = Arc::clone(&reached_next_test);
        let mut runner = Runner::new(Duration::from_millis(20));
        runner.register(TestCase::new("slow", |_| {
            std::thread::sleep(Duration::from_secs(1));
            Ok(())
        }));
        runner.register(TestCase::new("next", move |_| {
            next_test_marker.store(true, Ordering::Release);
            Ok(())
        }));
        let started = Instant::now();
        let report = runner.run(None);
        assert_eq!((report.passed, report.failed, report.skipped), (0, 1, 1));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!reached_next_test.load(Ordering::Acquire));
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message)
                if message.contains("worker ignored cancellation")
                    && message.contains("remaining tests were skipped")
        ));
    }
    #[test]
    fn per_test_timeout_overrides_runner_default() {
        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(
            TestCase::new("slow", |_| {
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            })
            .timeout(Duration::from_millis(20)),
        );
        let started = Instant::now();
        let report = runner.run(None);
        assert_eq!(report.failed, 1);
        assert!(started.elapsed() < Duration::from_millis(500));
    }
    #[test]
    fn long_test_names_are_kept_in_reports_without_becoming_thread_names() {
        let name = "a".repeat(10_000);
        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(TestCase::new(name.clone(), |_| Ok(())));

        let report = runner.run(None);

        assert_eq!(report.passed, 1);
        assert_eq!(report.tests[0].name, name);
    }
    #[test]
    fn typed_runner_rejects_invalid_timeouts_before_running_test_bodies() {
        let body_ran = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&body_ran);
        let mut runner = Runner::new(Duration::ZERO);
        runner.register(TestCase::new("zero default", move |_| {
            marker.store(true, Ordering::Release);
            Ok(())
        }));

        let report = runner.run(None);

        assert_eq!((report.passed, report.failed), (0, 1));
        assert!(!body_ran.load(Ordering::Acquire));
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("timeout must be greater than zero")
        ));

        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(TestCase::new("zero override", |_| Ok(())).timeout(Duration::ZERO));
        assert_eq!(runner.run(None).failed, 1);

        let body_ran = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&body_ran);
        let mut runner = Runner::new(Duration::MAX);
        runner.register(TestCase::new("excessive default", move |_| {
            marker.store(true, Ordering::Release);
            Ok(())
        }));
        let report = runner.run(None);
        assert_eq!(report.failed, 1);
        assert!(!body_ran.load(Ordering::Acquire));
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("at most 24 hours")
        ));

        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(TestCase::new("excessive override", |_| Ok(())).timeout(Duration::MAX));
        assert_eq!(runner.run(None).failed, 1);
    }
    #[test]
    fn run_config_rejects_invalid_programmatic_timeout() {
        let config = Config {
            tests: vec![ConfiguredTest {
                name: "must not run".into(),
                command: "unused".into(),
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };

        let report = run_config(
            config,
            RunOptions {
                timeout: Some(Duration::MAX),
                ..RunOptions::default()
            },
        );

        assert_eq!((report.selected, report.failed), (1, 1));
        assert!(matches!(
            &report.tests[0],
            TestResult { name, status: TestStatus::Failed(message), .. }
                if name == "configuration" && message.contains("at most 24 hours")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn configured_per_test_timeout_overrides_cli_suite_timeout() {
        let config = Config {
            timeout_ms: 1_000,
            tests: vec![ConfiguredTest {
                name: "per-test timeout".into(),
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 0.08".into()],
                timeout_ms: Some(300),
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(
            config,
            RunOptions {
                timeout: Some(Duration::from_millis(20)),
                ..RunOptions::default()
            },
        );

        assert_eq!((report.passed, report.failed), (1, 0), "{report:?}");
    }
    #[test]
    fn timeout_preserves_a_cooperative_late_body_error() {
        let mut runner = Runner::new(Duration::from_millis(20));
        runner.register(TestCase::new("diagnostic", |_| {
            std::thread::sleep(Duration::from_millis(50));
            Err(Error::Runner("specific assertion diagnostic".into()))
        }));
        let report = runner.run(None);
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message)
                if message.contains("timed out")
                    && message.contains("worker stopped after cancellation")
                    && message.contains("specific assertion diagnostic")
        ));
    }
    #[cfg(unix)]
    #[test]
    fn timeout_closes_a_session_to_unblock_the_test_body() {
        let body_unblocked = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&body_unblocked);
        let mut runner = Runner::new(Duration::from_millis(500));
        runner.register(TestCase::new("blocked on TUI", move |context| {
            let mut launch = LaunchOptions::new("/bin/sh").args(["-c", "sleep 10"]);
            launch.shutdown_timeout = Duration::from_millis(300);
            let session = context.tui(launch)?;
            while session.process().is_running()? {
                std::thread::sleep(Duration::from_millis(10));
            }
            marker.store(true, Ordering::Release);
            Ok(())
        }));
        let started = Instant::now();
        let report = runner.run(None);
        assert_eq!(report.failed, 1);
        assert!(body_unblocked.load(Ordering::Acquire));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn test_body_can_observe_timeout_cancellation() {
        let mut runner = Runner::new(Duration::from_millis(20));
        runner.register(TestCase::new("cooperative", |context| {
            while !context.is_cancelled() {
                std::thread::yield_now();
            }
            Ok(())
        }));
        let report = runner.run(None);
        assert_eq!(report.failed, 1);
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message)
                if message.contains("timed out")
                    && message.contains("stopped cooperatively after timeout cancellation")
        ));
    }
    #[test]
    fn panicking_test_is_reported_and_runner_continues() {
        let reached_next_test = Arc::new(AtomicBool::new(false));
        let next_test_marker = Arc::clone(&reached_next_test);
        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(TestCase::new("panic", |_| panic!("expected panic")));
        runner.register(TestCase::new("next", move |_| {
            next_test_marker.store(true, Ordering::Release);
            Ok(())
        }));
        let report = runner.run(None);
        assert_eq!((report.passed, report.failed), (1, 1));
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("expected panic")
        ));
        assert!(reached_next_test.load(Ordering::Acquire));
    }
}

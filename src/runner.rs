use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::session::{LaunchOptions, TuiSession};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
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
        let config: Self = toml::from_str(&source).map_err(|error| Error::Config {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if config.timeout_ms == 0 {
            return Err(Error::Config {
                path: path.to_path_buf(),
                message: "timeout_ms must be greater than zero".into(),
            });
        }
        for test in &config.tests {
            if test.name.trim().is_empty() || test.command.trim().is_empty() {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: "every [[tests]] entry requires non-empty name and command".into(),
                });
            }
            if test.timeout_ms == Some(0) {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!(
                        "timeout_ms for test `{}` must be greater than zero",
                        test.name
                    ),
                });
            }
            if test.expect_text.as_deref() == Some("") {
                return Err(Error::Config {
                    path: path.to_path_buf(),
                    message: format!("expect_text for test `{}` must not be empty", test.name),
                });
            }
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
    pub input: Option<String>,
    pub expect_text: Option<String>,
    /// Wait for any process exit. `expect_exit_code` implies this and also
    /// checks the exact code.
    pub expect_exit: bool,
    pub expect_exit_code: Option<i32>,
    pub skip: bool,
    pub focus: bool,
    pub timeout_ms: Option<u64>,
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
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub tests: Vec<TestResult>,
}

impl RunReport {
    pub fn success(&self) -> bool {
        self.failed == 0
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
    /// requested. Registered TUI sessions are closed by the runner once the
    /// test body returns.
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
    fn cleanup(&self) -> Result<()> {
        let mut first_error = None;
        let sessions: Vec<_> = self
            .sessions
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
}

type TestBody = Box<dyn FnOnce(&mut TestContext) -> Result<()> + Send>;

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
        for test in self.tests {
            let name = test.full_name();
            let filtered = grep.is_some_and(|pattern| !name.contains(pattern));
            if test.skip || filtered || (focused && !test.focus) {
                report.skipped += 1;
                report.tests.push(TestResult {
                    name,
                    status: TestStatus::Skipped,
                    duration: Duration::ZERO,
                });
                continue;
            }
            let started = Instant::now();
            let timeout = test.timeout.unwrap_or(self.timeout);
            let sessions = Arc::new(Mutex::new(Vec::new()));
            let cancelled = Arc::new(AtomicBool::new(false));
            let watchdog_cancelled = Arc::clone(&cancelled);
            let (finished_sender, finished_receiver) = mpsc::sync_channel(1);
            let watchdog = thread::Builder::new()
                .name(format!("ttry-watchdog-{name}"))
                .spawn(move || {
                    if matches!(
                        finished_receiver.recv_timeout(timeout),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        watchdog_cancelled.store(true, Ordering::Release);
                        true
                    } else {
                        false
                    }
                });
            let result = match watchdog {
                Ok(watchdog) => {
                    let mut context = TestContext {
                        sessions,
                        cancelled,
                        update_snapshots: self.update_snapshots,
                    };
                    let body_result = catch_unwind(AssertUnwindSafe(|| (test.body)(&mut context)));
                    let cleanup_result = context.cleanup();
                    let completed_result = match body_result {
                        Ok(result) => result.and(cleanup_result),
                        Err(payload) => {
                            let message = payload
                                .downcast_ref::<&str>()
                                .copied()
                                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                                .unwrap_or("unknown panic payload");
                            let cleanup = cleanup_result
                                .err()
                                .map(|error| format!("; cleanup also failed: {error}"))
                                .unwrap_or_default();
                            Err(Error::Runner(format!(
                                "test body panicked: {message}{cleanup}"
                            )))
                        }
                    };
                    let _ = finished_sender.send(());
                    match watchdog.join() {
                        Ok(true) => completed_result.and_then(|()| {
                            Err(Error::Timeout {
                                timeout,
                                context: format!("test `{name}` exceeded its timeout"),
                            })
                        }),
                        Ok(false) => completed_result,
                        Err(_) => Err(Error::Runner(format!(
                            "watchdog for test `{name}` panicked"
                        ))),
                    }
                }
                Err(error) => Err(Error::Runner(format!(
                    "could not start watchdog for test `{name}`: {error}"
                ))),
            };
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
    let cli_timeout = options.timeout;
    let timeout = cli_timeout.unwrap_or(Duration::from_millis(config.timeout_ms));
    let mut runner = Runner::new(timeout).update_snapshots(options.update_snapshots);
    for configured in config.tests {
        let name = configured.name.clone();
        let test_timeout = cli_timeout
            .or_else(|| configured.timeout_ms.map(Duration::from_millis))
            .unwrap_or(timeout);
        let group = configured.group.clone();
        let skip = configured.skip;
        let focus = configured.focus;
        let mut test = TestCase::new(name, move |context| {
            let mut launch = LaunchOptions::new(configured.command)
                .args(configured.args)
                .size(80, 24);
            launch.startup_timeout = test_timeout;
            launch.shutdown_timeout = Duration::from_millis(600);
            if let Some(cwd) = configured.cwd {
                launch = launch.cwd(cwd);
            }
            let session = context.tui(launch)?;
            if let Some(input) = configured.input {
                session.keyboard().paste(&input)?;
            }
            if let Some(expected) = configured.expect_text {
                session.wait_for_text(&expected, test_timeout)?;
            }
            if let Some(code) = configured.expect_exit_code {
                session
                    .expect_process()
                    .timeout(test_timeout)
                    .to_have_exited_with_code(code)?;
            } else if configured.expect_exit {
                session
                    .expect_process()
                    .timeout(test_timeout)
                    .to_have_exited()?;
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
    runner.run(options.grep.as_deref())
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
                ..ConfiguredTest::default()
            }],
            ..Config::default()
        };
        let report = run_config(config, RunOptions::default());
        assert_eq!((report.passed, report.failed), (1, 0), "{report:?}");
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
    fn non_cooperative_timeout_waits_for_body_before_next_test() {
        let reached_next_test = Arc::new(AtomicBool::new(false));
        let next_test_marker = Arc::clone(&reached_next_test);
        let mut runner = Runner::new(Duration::from_millis(20));
        runner.register(TestCase::new("slow", |_| {
            std::thread::sleep(Duration::from_millis(200));
            Ok(())
        }));
        runner.register(TestCase::new("next", move |_| {
            next_test_marker.store(true, Ordering::Release);
            Ok(())
        }));
        let started = Instant::now();
        let report = runner.run(None);
        assert_eq!(report.failed, 1);
        assert_eq!(report.passed, 1);
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(reached_next_test.load(Ordering::Acquire));
    }
    #[test]
    fn per_test_timeout_overrides_runner_default() {
        let mut runner = Runner::new(Duration::from_secs(1));
        runner.register(
            TestCase::new("slow", |_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .timeout(Duration::from_millis(20)),
        );
        let started = Instant::now();
        let report = runner.run(None);
        assert_eq!(report.failed, 1);
        assert!(started.elapsed() >= Duration::from_millis(150));
    }
    #[test]
    fn watchdog_preserves_a_specific_body_error() {
        let mut runner = Runner::new(Duration::from_millis(20));
        runner.register(TestCase::new("diagnostic", |_| {
            std::thread::sleep(Duration::from_millis(50));
            Err(Error::Runner("specific assertion diagnostic".into()))
        }));
        let report = runner.run(None);
        assert!(matches!(
            &report.tests[0].status,
            TestStatus::Failed(message) if message.contains("specific assertion diagnostic")
        ));
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

use std::time::{Duration, Instant};

use crate::locator::Locator;
use crate::process::{ProcessState, PtyProcess};
use crate::{Error, Result, Screen};

#[derive(Clone, Copy, Debug)]
pub struct ExpectOptions {
    pub timeout: Duration,
}

impl Default for ExpectOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
        }
    }
}

impl ExpectOptions {
    fn validate(&self) -> Result<()> {
        if self.timeout.is_zero() {
            return Err(Error::InvalidTimeout { field: "timeout" });
        }
        Ok(())
    }
}

/// Namespace for the default assertion timeout.
#[derive(Clone, Copy, Debug, Default)]
pub struct Expect;

pub trait IntoExpect {
    type Assertion;
    fn into_expect(self) -> Self::Assertion;
}

pub fn expect<T: IntoExpect>(target: T) -> T::Assertion {
    target.into_expect()
}

#[derive(Clone, Debug)]
pub struct LocatorExpect {
    locator: Locator,
    process: Option<PtyProcess>,
    options: ExpectOptions,
}

#[derive(Clone, Debug)]
pub struct ScreenExpect {
    screen: Screen,
    process: Option<PtyProcess>,
    options: ExpectOptions,
}

#[derive(Clone, Debug)]
pub struct ProcessExpect {
    process: PtyProcess,
    options: ExpectOptions,
}

impl IntoExpect for Locator {
    type Assertion = LocatorExpect;
    fn into_expect(self) -> Self::Assertion {
        LocatorExpect {
            locator: self,
            process: None,
            options: ExpectOptions::default(),
        }
    }
}
impl IntoExpect for Screen {
    type Assertion = ScreenExpect;
    fn into_expect(self) -> Self::Assertion {
        ScreenExpect {
            screen: self,
            process: None,
            options: ExpectOptions::default(),
        }
    }
}
impl IntoExpect for PtyProcess {
    type Assertion = ProcessExpect;
    fn into_expect(self) -> Self::Assertion {
        ProcessExpect {
            process: self,
            options: ExpectOptions::default(),
        }
    }
}

impl LocatorExpect {
    pub(crate) fn with_process(locator: Locator, process: PtyProcess) -> Self {
        Self {
            locator,
            process: Some(process),
            options: ExpectOptions::default(),
        }
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = timeout;
        self
    }
    pub fn to_be_visible(&self) -> Result<()> {
        self.retry("to be visible", || Ok(self.locator.is_visible()))
    }
    pub fn not_to_be_visible(&self) -> Result<()> {
        self.retry("not to be visible", || Ok(!self.locator.is_visible()))
    }
    pub fn to_have_text(&self, expected: &str) -> Result<()> {
        self.retry(&format!("to have text `{expected}`"), || {
            match self.locator.text() {
                Ok(actual) => Ok(actual == expected),
                Err(Error::StrictLocator { count: 0, .. }) => Ok(false),
                Err(error) => Err(error),
            }
        })
    }
    pub fn to_have_count(&self, expected: usize) -> Result<()> {
        self.retry(&format!("to have count {expected}"), || {
            Ok(self.locator.count() == expected)
        })
    }

    fn retry(&self, expectation: &str, predicate: impl Fn() -> Result<bool>) -> Result<()> {
        self.options.validate()?;
        let deadline = Instant::now() + self.options.timeout;
        loop {
            if predicate()? {
                return Ok(());
            }
            if let Some(process) = &self.process {
                if process.output_drained() {
                    if let ProcessState::Exited(status) = process.state()? {
                        return Err(Error::ProcessExited(format!(
                            "{status}; expected locator {} {expectation}",
                            self.locator.describe()
                        )));
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout {
                    timeout: self.options.timeout,
                    context: format!(
                        "expected locator {} {expectation}; current screen:\n{}",
                        self.locator.describe(),
                        self.locator.screen().text()
                    ),
                });
            }
            let version = self.locator.screen().version();
            self.locator
                .screen()
                .wait_for_change(version, (deadline - now).min(Duration::from_millis(50)));
        }
    }
}

impl ScreenExpect {
    pub(crate) fn with_process(screen: Screen, process: PtyProcess) -> Self {
        Self {
            screen,
            process: Some(process),
            options: ExpectOptions::default(),
        }
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = timeout;
        self
    }
    pub fn to_contain_text(&self, expected: &str) -> Result<()> {
        self.options.validate()?;
        let deadline = Instant::now() + self.options.timeout;
        loop {
            if self.screen.text().contains(expected) {
                return Ok(());
            }
            if let Some(process) = &self.process {
                if process.output_drained() {
                    if let ProcessState::Exited(status) = process.state()? {
                        return Err(Error::ProcessExited(status.to_string()));
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout {
                    timeout: self.options.timeout,
                    context: format!(
                        "expected screen to contain `{expected}`; actual:\n{}",
                        self.screen.text()
                    ),
                });
            }
            let version = self.screen.version();
            self.screen
                .wait_for_change(version, (deadline - now).min(Duration::from_millis(50)));
        }
    }
}

impl ProcessExpect {
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = timeout;
        self
    }
    pub fn to_be_running(&self) -> Result<()> {
        self.options.validate()?;
        match self.process.state()? {
            ProcessState::Running => Ok(()),
            ProcessState::Exited(status) => Err(Error::ProcessExited(format!(
                "expected running, got {status}"
            ))),
        }
    }
    pub fn to_have_exited(&self) -> Result<()> {
        self.wait_for_exit(None)
    }
    pub fn to_have_exited_with_code(&self, expected: i32) -> Result<()> {
        self.wait_for_exit(Some(expected))
    }
    fn wait_for_exit(&self, expected: Option<i32>) -> Result<()> {
        self.options.validate()?;
        let deadline = Instant::now() + self.options.timeout;
        loop {
            if let ProcessState::Exited(status) = self.process.state()? {
                if expected.is_none() || status.code == expected {
                    return Ok(());
                }
                return Err(Error::ProcessExited(format!(
                    "expected exit code {}, got {status}",
                    expected.unwrap()
                )));
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout {
                    timeout: self.options.timeout,
                    context: format!(
                        "waiting for process exit; recent events: {:?}",
                        self.process.recent_events()
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;
    #[test]
    fn visible_text_and_count_assertions() {
        let mut terminal = Terminal::new(20, 1).unwrap();
        terminal.advance(b"ready ready");
        expect(terminal.screen().get_by_text("ready"))
            .to_have_count(2)
            .unwrap();
        expect(terminal.screen()).to_contain_text("ready").unwrap();
    }

    #[test]
    fn strict_text_errors_are_not_retried_as_timeouts() {
        let mut terminal = Terminal::new(20, 1).unwrap();
        terminal.advance(b"ready ready");
        let result = expect(terminal.screen().get_by_text("ready"))
            .timeout(Duration::from_secs(1))
            .to_have_text("ready");
        assert!(matches!(result, Err(Error::StrictLocator { count: 2, .. })));
    }

    #[test]
    fn missing_text_is_retried_until_timeout() {
        let terminal = Terminal::new(20, 1).unwrap();
        let result = expect(terminal.screen().get_by_text("ready"))
            .timeout(Duration::from_millis(10))
            .to_have_text("ready");
        assert!(matches!(result, Err(Error::Timeout { .. })));
    }

    #[test]
    fn zero_assertion_timeout_is_rejected_before_sampling() {
        let mut terminal = Terminal::new(20, 1).unwrap();
        terminal.advance(b"ready");

        let locator_result = expect(terminal.screen().get_by_text("ready"))
            .timeout(Duration::ZERO)
            .to_be_visible();
        let screen_result = expect(terminal.screen())
            .timeout(Duration::ZERO)
            .to_contain_text("ready");

        assert!(matches!(
            locator_result,
            Err(Error::InvalidTimeout { field: "timeout" })
        ));
        assert!(matches!(
            screen_result,
            Err(Error::InvalidTimeout { field: "timeout" })
        ));
    }
}

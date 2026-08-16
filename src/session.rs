use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{deadline, validate_timeout};
use crate::expect::{LocatorExpect, ProcessExpect, ScreenExpect};
use crate::keyboard::Keyboard;
use crate::process::{ProcessState, PtyOptions, PtyProcess};
use crate::screen::validate_dimensions;
use crate::{Error, Locator, Rect, Result, Screen, Terminal};

fn ingest_pty_output(
    reader: &mut dyn Read,
    terminal: &mut Terminal,
    output_lock: &Mutex<()>,
) -> std::io::Result<()> {
    let mut bytes = [0_u8; 8192];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                let _guard = output_lock.lock().expect("output lock poisoned");
                terminal.advance(&bytes[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub command: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub term: String,
    pub cols: u16,
    pub rows: u16,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl LaunchOptions {
    pub fn new(command: impl Into<OsString>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: std::env::current_dir().ok(),
            env: BTreeMap::new(),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
            startup_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_millis(600),
        }
    }
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }
}

struct SessionInner {
    process: PtyProcess,
    keyboard: Keyboard,
    screen: Screen,
    output_lock: Arc<Mutex<()>>,
    reader: Mutex<ReaderLifecycle>,
    reader_shutdown_timeout: Duration,
}

enum ReaderLifecycle {
    Running(thread::JoinHandle<()>),
    Joined,
    Panicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReaderJoinError {
    Timeout(Duration),
    Panicked,
}

impl std::fmt::Display for ReaderJoinError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(timeout) => {
                write!(formatter, "PTY reader did not stop within {timeout:?}")
            }
            Self::Panicked => write!(formatter, "PTY reader thread panicked"),
        }
    }
}

fn join_reader_until(
    reader: &Mutex<ReaderLifecycle>,
    timeout: Duration,
) -> std::result::Result<(), ReaderJoinError> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut lifecycle = reader.lock().expect("reader lock poisoned");
        match &*lifecycle {
            ReaderLifecycle::Joined => return Ok(()),
            ReaderLifecycle::Panicked => {
                return Err(ReaderJoinError::Panicked);
            }
            ReaderLifecycle::Running(handle) if handle.is_finished() => {
                // The mutex remains held for this nonblocking join, so no
                // intermediate lifecycle state can become observable.
                let current = std::mem::replace(&mut *lifecycle, ReaderLifecycle::Joined);
                let ReaderLifecycle::Running(handle) = current else {
                    unreachable!();
                };
                // is_finished guarantees this join will not wait for thread
                // execution.
                if handle.join().is_err() {
                    *lifecycle = ReaderLifecycle::Panicked;
                    return Err(ReaderJoinError::Panicked);
                }
                return Ok(());
            }
            ReaderLifecycle::Running(_) => {}
        }
        drop(lifecycle);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ReaderJoinError::Timeout(timeout));
        }
        thread::sleep(Duration::from_millis(5).min(remaining));
    }
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        let _ = self.process.close();
        // Drop cannot report reader failures, but it still gives the reader a
        // bounded opportunity to finish so a just-completed thread is joined
        // instead of detached.
        let _ = join_reader_until(&self.reader, self.reader_shutdown_timeout);
    }
}

impl SessionInner {
    fn close(&self) -> Result<()> {
        self.process.close()?;
        // A successful session close guarantees both process-tree cleanup and
        // reader termination. The join remains bounded; incomplete reader
        // shutdown is reported instead of silently returning a partial close.
        match join_reader_until(&self.reader, self.reader_shutdown_timeout) {
            Ok(()) => self.process.record_event("PTY reader joined"),
            Err(ReaderJoinError::Timeout(timeout)) => {
                return Err(Error::Runner(ReaderJoinError::Timeout(timeout).to_string()));
            }
            Err(ReaderJoinError::Panicked) => {
                return Err(Error::Runner(ReaderJoinError::Panicked.to_string()));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct TuiSession {
    inner: Arc<SessionInner>,
}

impl std::fmt::Debug for TuiSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiSession")
            .field("process", &self.inner.process)
            .field("dimensions", &self.inner.screen.dimensions())
            .finish()
    }
}

pub fn launch(command: impl Into<OsString>) -> Result<TuiSession> {
    TuiSession::launch(LaunchOptions::new(command))
}

impl TuiSession {
    pub fn launch(options: LaunchOptions) -> Result<Self> {
        validate_timeout(options.startup_timeout, "startup_timeout")?;
        validate_timeout(options.shutdown_timeout, "shutdown_timeout")?;
        let startup_timeout = options.startup_timeout;
        let shutdown_timeout = options.shutdown_timeout;
        // Construct every fallible in-memory component before spawning the
        // external process. Later error paths are additionally protected by
        // PtyProcess's bounded Drop cleanup.
        let mut terminal = Terminal::new(options.cols, options.rows)?;
        let mut pty_options = PtyOptions::new(options.command);
        pty_options.args = options.args;
        pty_options.cwd = options.cwd;
        pty_options.env = options.env;
        pty_options.term = options.term;
        pty_options.cols = options.cols;
        pty_options.rows = options.rows;
        pty_options.shutdown_timeout = shutdown_timeout;
        let startup_started = Instant::now();
        let (process, mut reader) = PtyProcess::spawn(pty_options)?;
        if startup_started.elapsed() > startup_timeout {
            let _ = process.close();
            return Err(Error::Timeout {
                timeout: startup_timeout,
                context: "starting PTY process".into(),
            });
        }
        let screen = terminal.screen();
        let thread_screen = screen.clone();
        let thread_process = process.clone();
        let output_lock = Arc::new(Mutex::new(()));
        let reader_output_lock = Arc::clone(&output_lock);
        let reader = thread::Builder::new()
            .name("ttry-pty-reader".into())
            .spawn(move || {
                if let Err(error) =
                    ingest_pty_output(reader.as_mut(), &mut terminal, &reader_output_lock)
                {
                    thread_process.record_event(format!("PTY reader failed: {error}"));
                }
                thread_process.mark_output_drained();
                let _ = thread_process.state();
                thread_screen.notify();
            })?;
        let keyboard = Keyboard::new(process.writer());
        Ok(Self {
            inner: Arc::new(SessionInner {
                process,
                keyboard,
                screen,
                output_lock,
                reader: Mutex::new(ReaderLifecycle::Running(reader)),
                reader_shutdown_timeout: shutdown_timeout,
            }),
        })
    }

    pub fn keyboard(&self) -> &Keyboard {
        &self.inner.keyboard
    }
    pub fn screen(&self) -> &Screen {
        &self.inner.screen
    }
    pub fn process(&self) -> &PtyProcess {
        &self.inner.process
    }
    pub fn get_by_text(&self, text: impl Into<String>) -> Locator {
        self.inner.screen.get_by_text(text)
    }
    pub fn region(&self, rect: Rect) -> Result<Screen> {
        self.inner.screen.region(rect)
    }
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        validate_dimensions(cols, rows)?;
        let _guard = self.inner.output_lock.lock().expect("output lock poisoned");
        self.inner.process.resize(cols, rows)?;
        self.inner.screen.resize(cols, rows)
    }
    pub fn close(&self) -> Result<()> {
        self.inner.close()
    }
    pub fn expect(&self, locator: Locator) -> LocatorExpect {
        LocatorExpect::with_process(locator, self.inner.process.clone())
    }
    pub fn expect_screen(&self) -> ScreenExpect {
        ScreenExpect::with_process(self.inner.screen.clone(), self.inner.process.clone())
    }
    pub fn expect_process(&self) -> ProcessExpect {
        crate::expect(self.inner.process.clone())
    }

    pub fn wait_for_text(&self, text: &str, timeout: Duration) -> Result<()> {
        let locator = self.get_by_text(text);
        let deadline = deadline(timeout, "timeout")?;
        let mut initial_sample = true;
        loop {
            let version = self.inner.screen.version();
            if !initial_sample && Instant::now() >= deadline {
                return Err(self.text_timeout(text, timeout));
            }
            let matched = locator.is_visible();
            let sampled_at = Instant::now();
            if matched {
                if initial_sample || sampled_at < deadline {
                    return Ok(());
                }
                return Err(self.text_timeout(text, timeout));
            }
            if sampled_at >= deadline {
                return Err(self.text_timeout(text, timeout));
            }
            if self.inner.process.output_drained() {
                if let ProcessState::Exited(status) = self.inner.process.state()? {
                    return Err(Error::ProcessExited(status.to_string()));
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(self.text_timeout(text, timeout));
            }
            initial_sample = false;
            self.inner
                .screen
                .wait_for_change(version, (deadline - now).min(Duration::from_millis(50)));
        }
    }

    fn text_timeout(&self, text: &str, timeout: Duration) -> Error {
        Error::Timeout {
            timeout,
            context: format!(
                "waiting for text `{text}`; screen:\n{}",
                self.inner.screen.text()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InterruptedThenData {
        interrupted: bool,
        emitted: bool,
    }

    impl Read for InterruptedThenData {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            if self.emitted {
                return Ok(0);
            }
            self.emitted = true;
            let bytes = b"ready";
            buffer[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    #[test]
    fn pty_ingestion_retries_an_interrupted_read() {
        let mut reader = InterruptedThenData {
            interrupted: false,
            emitted: false,
        };
        let mut terminal = Terminal::new(10, 1).unwrap();
        ingest_pty_output(&mut reader, &mut terminal, &Mutex::new(())).unwrap();
        assert_eq!(terminal.screen().text(), "ready");
    }

    #[test]
    fn reader_join_timeout_is_bounded_and_retryable() {
        let reader = Mutex::new(ReaderLifecycle::Running(thread::spawn(|| {
            thread::sleep(Duration::from_millis(80));
        })));
        let started = Instant::now();
        assert!(matches!(
            join_reader_until(&reader, Duration::from_millis(10)),
            Err(ReaderJoinError::Timeout(timeout)) if timeout == Duration::from_millis(10)
        ));
        assert!(started.elapsed() < Duration::from_millis(60));
        thread::sleep(Duration::from_millis(90));
        join_reader_until(&reader, Duration::from_millis(10)).unwrap();
    }

    #[test]
    fn reader_panic_remains_visible_to_later_close_attempts() {
        let reader = Mutex::new(ReaderLifecycle::Running(thread::spawn(|| {
            panic!("reader failed");
        })));
        for _ in 0..2 {
            assert!(matches!(
                join_reader_until(&reader, Duration::from_secs(1)),
                Err(ReaderJoinError::Panicked)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn explicit_session_close_reports_a_reader_panic() {
        let session = TuiSession::launch(
            LaunchOptions::new("/bin/sh")
                .args(["-c", "exit 0"])
                .size(20, 2),
        )
        .unwrap();
        session.inner.process.close().unwrap();
        join_reader_until(&session.inner.reader, Duration::from_secs(1)).unwrap();
        *session.inner.reader.lock().expect("reader lock poisoned") = ReaderLifecycle::Panicked;

        assert!(matches!(
            session.close(),
            Err(Error::Runner(message)) if message.contains("PTY reader thread panicked")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn session_close_retries_reader_join_after_process_cleanup_completed() {
        let mut options = LaunchOptions::new("/bin/sh")
            .args(["-c", "exit 0"])
            .size(20, 2);
        options.shutdown_timeout = Duration::from_millis(10);
        let session = TuiSession::launch(options).unwrap();
        session.inner.process.close().unwrap();
        join_reader_until(&session.inner.reader, Duration::from_secs(1)).unwrap();
        *session.inner.reader.lock().expect("reader lock poisoned") =
            ReaderLifecycle::Running(thread::spawn(|| {
                thread::sleep(Duration::from_millis(80));
            }));

        assert!(matches!(
            session.close(),
            Err(Error::Runner(message)) if message.contains("PTY reader did not stop")
        ));
        thread::sleep(Duration::from_millis(90));
        session.close().unwrap();
    }
}

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use crate::expect::{LocatorExpect, ProcessExpect, ScreenExpect};
use crate::keyboard::Keyboard;
use crate::process::{ProcessState, PtyOptions, PtyProcess};
use crate::{Error, Locator, Rect, Result, Screen, Terminal};

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
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        let _ = self.process.close();
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
        if options.startup_timeout.is_zero() {
            return Err(Error::Timeout {
                timeout: options.startup_timeout,
                context: "starting PTY process".into(),
            });
        }
        let startup_timeout = options.startup_timeout;
        let mut pty_options = PtyOptions::new(options.command);
        pty_options.args = options.args;
        pty_options.cwd = options.cwd;
        pty_options.env = options.env;
        pty_options.term = options.term;
        pty_options.cols = options.cols;
        pty_options.rows = options.rows;
        pty_options.shutdown_timeout = options.shutdown_timeout;
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("ttry-pty-launcher".into())
            .spawn(move || {
                let _ = startup_sender.send(PtyProcess::spawn(pty_options));
            })?;
        let (process, mut reader) = match startup_receiver.recv_timeout(startup_timeout) {
            Ok(result) => result?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(Error::Timeout {
                    timeout: startup_timeout,
                    context: "starting PTY process".into(),
                })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Runner("PTY launcher stopped unexpectedly".into()))
            }
        };
        let mut terminal = Terminal::new(options.cols, options.rows)?;
        let screen = terminal.screen();
        let thread_screen = screen.clone();
        let thread_process = process.clone();
        thread::Builder::new()
            .name("ttry-pty-reader".into())
            .spawn(move || {
                let mut bytes = [0_u8; 8192];
                loop {
                    match reader.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => terminal.advance(&bytes[..count]),
                    }
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
        self.inner.process.resize(cols, rows)?;
        self.inner.screen.resize(cols, rows)
    }
    pub fn close(&self) -> Result<()> {
        self.inner.process.close()
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
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if locator.is_visible() {
                return Ok(());
            }
            if self.inner.process.output_drained() {
                if let ProcessState::Exited(status) = self.inner.process.state() {
                    return Err(Error::ProcessExited(status.to_string()));
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(Error::Timeout {
                    timeout,
                    context: format!(
                        "waiting for text `{text}`; screen:\n{}",
                        self.inner.screen.text()
                    ),
                });
            }
            let version = self.inner.screen.version();
            self.inner
                .screen
                .wait_for_change(version, (deadline - now).min(Duration::from_millis(50)));
        }
    }
}

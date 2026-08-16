use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::sys::signal::{killpg, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct PtyOptions {
    pub command: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub term: String,
    pub cols: u16,
    pub rows: u16,
    pub shutdown_timeout: Duration,
}

impl PtyOptions {
    pub fn new(command: impl Into<OsString>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: std::env::current_dir().ok(),
            env: BTreeMap::new(),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
            shutdown_timeout: Duration::from_millis(500),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.code, &self.signal) {
            (_, Some(signal)) => write!(f, "signal {signal}"),
            (Some(code), None) => write!(f, "exit code {code}"),
            _ => write!(f, "unknown exit status"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessState {
    Running,
    Exited(ExitStatus),
}

struct ProcessInner {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    close_lock: Mutex<()>,
    exit: Mutex<Option<ExitStatus>>,
    closed: AtomicBool,
    output_drained: AtomicBool,
    command: String,
    shutdown_timeout: Duration,
    events: Mutex<Vec<String>>,
    #[cfg(unix)]
    process_group: Option<i32>,
}

#[derive(Clone)]
pub struct PtyProcess {
    inner: Arc<ProcessInner>,
}

impl std::fmt::Debug for PtyProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyProcess")
            .field("command", &self.inner.command)
            .field("state", &self.state())
            .finish()
    }
}

impl PtyProcess {
    pub fn spawn(options: PtyOptions) -> Result<(Self, Box<dyn Read + Send>)> {
        if options.cols == 0 || options.rows == 0 {
            return Err(Error::InvalidDimensions {
                cols: options.cols,
                rows: options.rows,
            });
        }
        let command_display = options.command.to_string_lossy().into_owned();
        let command_path = std::path::Path::new(&options.command);
        let resolved_cwd = options
            .cwd
            .as_deref()
            .map(|cwd| {
                if cwd.is_absolute() {
                    Ok(cwd.to_path_buf())
                } else {
                    std::env::current_dir()
                        .map(|current| current.join(cwd))
                        .map_err(|source| Error::Launch {
                            command: command_display.clone(),
                            source: source.into(),
                        })
                }
            })
            .transpose()?;
        let executable = if !command_path.is_absolute() && command_path.components().count() > 1 {
            resolved_cwd
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(command_path)
                .into_os_string()
        } else {
            options.command.clone()
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: options.rows,
                cols: options.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|source| Error::Launch {
                command: command_display.clone(),
                source: source.into(),
            })?;
        let mut command = CommandBuilder::new(executable);
        command.args(&options.args);
        command.env("TERM", &options.term);
        command.env("COLUMNS", options.cols.to_string());
        command.env("LINES", options.rows.to_string());
        if let Some(cwd) = &resolved_cwd {
            command.cwd(cwd);
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|source| Error::Launch {
                command: command_display.clone(),
                source: source.into(),
            })?;
        #[cfg(unix)]
        let process_group = child.process_id().and_then(|pid| i32::try_from(pid).ok());
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|source| Error::Launch {
                command: command_display.clone(),
                source: source.into(),
            })?;
        let writer = pair.master.take_writer().map_err(|source| Error::Launch {
            command: command_display.clone(),
            source: source.into(),
        })?;
        let process = Self {
            inner: Arc::new(ProcessInner {
                master: Mutex::new(pair.master),
                writer: Arc::new(Mutex::new(writer)),
                child: Mutex::new(child),
                close_lock: Mutex::new(()),
                exit: Mutex::new(None),
                closed: AtomicBool::new(false),
                output_drained: AtomicBool::new(false),
                command: command_display,
                shutdown_timeout: options.shutdown_timeout,
                events: Mutex::new(vec!["process launched".into()]),
                #[cfg(unix)]
                process_group,
            }),
        };
        Ok((process, reader))
    }

    pub fn state(&self) -> Result<ProcessState> {
        let mut cached_exit = self.inner.exit.lock().expect("exit lock poisoned");
        if let Some(status) = cached_exit.clone() {
            return Ok(ProcessState::Exited(status));
        }
        let mut child = self.inner.child.lock().expect("child lock poisoned");
        match child.try_wait().map_err(Error::Io)? {
            Some(status) => {
                let normalized = normalize_status(status);
                *cached_exit = Some(normalized.clone());
                self.inner
                    .events
                    .lock()
                    .expect("events lock poisoned")
                    .push(format!("process exited: {normalized}"));
                Ok(ProcessState::Exited(normalized))
            }
            None => Ok(ProcessState::Running),
        }
    }

    pub fn is_running(&self) -> Result<bool> {
        Ok(matches!(self.state()?, ProcessState::Running))
    }
    pub(crate) fn output_drained(&self) -> bool {
        self.inner.output_drained.load(Ordering::Acquire)
    }
    pub(crate) fn mark_output_drained(&self) {
        self.inner.output_drained.store(true, Ordering::Release);
    }
    pub fn process_id(&self) -> Option<u32> {
        self.inner
            .child
            .lock()
            .expect("child lock poisoned")
            .process_id()
    }
    pub fn recent_events(&self) -> Vec<String> {
        self.inner
            .events
            .lock()
            .expect("events lock poisoned")
            .clone()
    }
    pub(crate) fn writer(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        Arc::clone(&self.inner.writer)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 {
            return Err(Error::InvalidDimensions { cols, rows });
        }
        self.inner
            .master
            .lock()
            .expect("master lock poisoned")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| Error::Io(std::io::Error::other(error)))?;
        self.inner
            .events
            .lock()
            .expect("events lock poisoned")
            .push(format!("resized to {cols}x{rows}"));
        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        let _close_guard = self.inner.close_lock.lock().expect("close lock poisoned");
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.is_running()? {
            self.inner
                .events
                .lock()
                .expect("events lock poisoned")
                .push("graceful close requested".into());
            let _ = self
                .inner
                .writer
                .lock()
                .expect("writer lock poisoned")
                .write_all(&[0x04]);
        }
        if self.wait_until_tree_exit(self.inner.shutdown_timeout / 3)? {
            self.inner.closed.store(true, Ordering::Release);
            return Ok(());
        }

        #[cfg(unix)]
        {
            self.inner
                .events
                .lock()
                .expect("events lock poisoned")
                .push("SIGTERM sent to process group".into());
            self.signal_process_group(Signal::SIGTERM)?;
            if self.wait_until_tree_exit(self.inner.shutdown_timeout / 3)? {
                self.inner.closed.store(true, Ordering::Release);
                return Ok(());
            }
        }

        self.inner
            .events
            .lock()
            .expect("events lock poisoned")
            .push("forced termination requested".into());

        #[cfg(unix)]
        self.signal_process_group(Signal::SIGKILL)?;

        if self.is_running()? {
            self.inner
                .child
                .lock()
                .expect("child lock poisoned")
                .kill()
                .map_err(|error| Error::Io(std::io::Error::other(error)))?;
        }

        if self.wait_until_tree_exit(self.inner.shutdown_timeout / 3)? {
            self.inner.closed.store(true, Ordering::Release);
            Ok(())
        } else {
            Err(Error::Runner(format!(
                "process `{}` did not exit after forced termination",
                self.inner.command
            )))
        }
    }

    fn wait_until_tree_exit(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let leader_exited = !self.is_running()?;
            #[cfg(unix)]
            let group_exited = !self.process_group_is_running();
            #[cfg(not(unix))]
            let group_exited = true;
            if leader_exited && group_exited {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(10).min(remaining));
        }
    }

    #[cfg(unix)]
    fn signal_process_group(&self, signal: Signal) -> Result<()> {
        let Some(process_group) = self.inner.process_group else {
            return Ok(());
        };
        match killpg(Pid::from_raw(process_group), signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(Error::Io(std::io::Error::from_raw_os_error(error as i32))),
        }
    }

    #[cfg(unix)]
    fn process_group_is_running(&self) -> bool {
        self.inner.process_group.is_some_and(|process_group| {
            match killpg(Pid::from_raw(process_group), None::<Signal>) {
                Ok(()) => true,
                Err(Errno::ESRCH) => false,
                Err(_) => true,
            }
        })
    }
}

fn normalize_status(status: portable_pty::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status
            .signal()
            .is_none()
            .then_some(status.exit_code() as i32),
        signal: status.signal().map(str::to_owned),
    }
}

impl Drop for ProcessInner {
    fn drop(&mut self) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let child = self.child.get_mut().expect("child lock poisoned");
        if matches!(child.try_wait(), Ok(Some(_))) {
            #[cfg(not(unix))]
            return;
        }

        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
        }
        let _ = child.kill();
        let deadline = Instant::now() + self.shutdown_timeout;
        loop {
            let leader_exited = matches!(child.try_wait(), Ok(Some(_)));
            #[cfg(unix)]
            let group_exited = self.process_group.is_none_or(|process_group| {
                matches!(
                    killpg(Pid::from_raw(process_group), None::<Signal>),
                    Err(Errno::ESRCH)
                )
            });
            #[cfg(not(unix))]
            let group_exited = true;
            if leader_exited && group_exited {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(Duration::from_millis(10).min(remaining));
        }
    }
}

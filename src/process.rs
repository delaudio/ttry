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
use nix::libc;
#[cfg(unix)]
use nix::sys::signal::{killpg, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::screen::validate_dimensions;
use crate::{Error, Result};

#[cfg(unix)]
enum ExitObservation {
    Running,
    Exited(ExitStatus),
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    ExitedWithoutStatus,
}

#[cfg(target_vendor = "apple")]
enum ProcState {
    Live,
    Zombie,
    Gone,
}

#[cfg(any(target_vendor = "apple", target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupLiveness {
    Live,
    Empty,
    Inconclusive,
}

#[cfg(target_vendor = "apple")]
fn libproc_error_liveness(error: &std::io::Error) -> GroupLiveness {
    if error.raw_os_error() == Some(libc::ESRCH) {
        GroupLiveness::Empty
    } else {
        GroupLiveness::Inconclusive
    }
}

#[cfg(target_vendor = "apple")]
fn observe_proc_state(pid: i32) -> Result<ProcState> {
    // SAFETY: proc_pidinfo receives a correctly sized writable proc_bsdinfo
    // buffer. It observes process state without waiting or reaping.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let expected = std::mem::size_of::<libc::proc_bsdinfo>();
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            expected as libc::c_int,
        )
    };
    if written == expected as libc::c_int {
        return Ok(if info.pbi_status == libc::SZOMB {
            ProcState::Zombie
        } else {
            ProcState::Live
        });
    }
    if written == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(ProcState::Gone);
        }
        return Err(Error::Io(error));
    }
    Err(Error::Io(std::io::Error::other(format!(
        "proc_pidinfo returned {written} bytes; expected {expected}"
    ))))
}

#[cfg(target_vendor = "apple")]
fn process_group_liveness(process_group: i32) -> Result<GroupLiveness> {
    let mut capacity = 64_usize;
    loop {
        let mut inconclusive = false;
        let mut pids = vec![0_i32; capacity];
        let buffer_bytes = pids
            .len()
            .checked_mul(std::mem::size_of::<i32>())
            .and_then(|bytes| libc::c_int::try_from(bytes).ok())
            .ok_or_else(|| Error::Runner("process-group PID buffer is too large".into()))?;
        // SAFETY: pids is writable for buffer_bytes bytes and
        // proc_listpgrppids only fills the provided PID array.
        let pid_count = unsafe {
            libc::proc_listpgrppids(process_group, pids.as_mut_ptr().cast(), buffer_bytes)
        };
        if pid_count < 0 {
            return Ok(libproc_error_liveness(&std::io::Error::last_os_error()));
        }
        // The buffersize argument is bytes, but proc_listpgrppids returns the
        // number of PIDs written (for example, three PIDs returns 3 for a
        // 12-byte payload), unlike byte-count-returning libproc calls.
        if usize::try_from(pid_count).ok() == Some(capacity) {
            capacity = capacity
                .checked_mul(2)
                .ok_or_else(|| Error::Runner("process-group PID buffer overflow".into()))?;
            continue;
        }
        let count = usize::try_from(pid_count).unwrap_or(0);
        if count > capacity {
            return Err(Error::Runner(format!(
                "proc_listpgrppids returned {count} PIDs for a {capacity}-PID buffer"
            )));
        }
        for &pid in &pids[..count] {
            if pid <= 0 {
                continue;
            }
            match observe_proc_state(pid) {
                Ok(ProcState::Live) => return Ok(GroupLiveness::Live),
                Ok(ProcState::Zombie | ProcState::Gone) => {}
                Err(_) => inconclusive = true,
            }
        }
        return Ok(if inconclusive {
            GroupLiveness::Inconclusive
        } else {
            GroupLiveness::Empty
        });
    }
}

#[cfg(target_os = "linux")]
fn process_group_liveness(process_group: i32) -> Result<GroupLiveness> {
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        // killpg has already established that the group exists. A missing or
        // restricted procfs cannot disprove that, so keep cleanup proceeding
        // conservatively instead of making procfs a hard dependency.
        Err(_) => return Ok(GroupLiveness::Inconclusive),
    };
    let mut inconclusive = false;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            // Once killpg has established that the group exists, an
            // incomplete /proc scan cannot safely prove that it contains no
            // live members. Keep cleanup conservative in that case.
            Err(_) => {
                inconclusive = true;
                continue;
            }
        };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            // A PID can disappear while /proc is being scanned. Every other
            // failure makes the scan incomplete, so it must not override the
            // successful killpg existence check performed by the caller.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                match nix::unistd::getpgid(Some(Pid::from_raw(pid))) {
                    Ok(group) if group.as_raw() == process_group => {
                        return Ok(GroupLiveness::Live);
                    }
                    Ok(_) | Err(Errno::ESRCH) => {}
                    Err(_) => inconclusive = true,
                }
                continue;
            }
        };
        let Some(after_name) = stat.rsplit_once(')').map(|(_, fields)| fields) else {
            match nix::unistd::getpgid(Some(Pid::from_raw(pid))) {
                Ok(group) if group.as_raw() == process_group => {
                    return Ok(GroupLiveness::Live);
                }
                Ok(_) | Err(Errno::ESRCH) => {}
                Err(_) => inconclusive = true,
            }
            continue;
        };
        let mut fields = after_name.split_whitespace();
        let state = fields.next();
        let _parent = fields.next();
        let group = fields.next().and_then(|value| value.parse::<i32>().ok());
        if state.is_none() || group.is_none() {
            match nix::unistd::getpgid(Some(Pid::from_raw(pid))) {
                Ok(group) if group.as_raw() == process_group => {
                    return Ok(GroupLiveness::Live);
                }
                Ok(_) | Err(Errno::ESRCH) => {}
                Err(_) => inconclusive = true,
            }
            continue;
        }
        if group == Some(process_group) && state != Some("Z") {
            return Ok(GroupLiveness::Live);
        }
    }
    Ok(if inconclusive {
        GroupLiveness::Inconclusive
    } else {
        GroupLiveness::Empty
    })
}

#[cfg(target_vendor = "apple")]
fn decode_kqueue_exit(observed: libc::c_int, event: &libc::kevent) -> Result<ExitObservation> {
    if observed > 0 && event.flags & libc::EV_ERROR != 0 {
        let errno = i32::try_from(event.data).unwrap_or(libc::EIO);
        return Err(Error::Io(std::io::Error::from_raw_os_error(
            if errno == 0 { libc::EIO } else { errno },
        )));
    }
    if observed == 0 || event.fflags & libc::NOTE_EXIT == 0 {
        return Ok(ExitObservation::Running);
    }
    let status = i32::try_from(event.data)
        .map_err(|_| Error::Runner("kqueue returned an invalid child exit status".into()))?;
    if libc::WIFEXITED(status) {
        return Ok(ExitObservation::Exited(ExitStatus {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }));
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        return Ok(ExitObservation::Exited(ExitStatus {
            code: None,
            signal: Some(
                Signal::try_from(signal)
                    .map(|signal| format!("{signal:?}"))
                    .unwrap_or_else(|_| format!("signal {signal}")),
            ),
        }));
    }
    Ok(ExitObservation::ExitedWithoutStatus)
}

#[cfg(target_vendor = "apple")]
fn observe_exit_with_kqueue(pid: i32) -> Result<ExitObservation> {
    // kqueue observes NOTE_EXIT without consuming the wait status, preserving
    // the leader PID/PGID until cleanup. NOTE_EXITSTATUS returns wait(2)-style
    // status data for a child process.
    let queue = unsafe { libc::kqueue() };
    if queue == -1 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: queue is a valid kqueue descriptor, and all pointers reference
    // initialized structures for exactly the counts supplied.
    let observed = unsafe { libc::kevent(queue, &change, 1, &mut event, 1, &timeout) };
    let error = (observed == -1).then(std::io::Error::last_os_error);
    unsafe {
        libc::close(queue);
    }
    if let Some(error) = error {
        return Err(Error::Io(error));
    }
    decode_kqueue_exit(observed, &event)
}

#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn decode_waitid_exit(si_code: libc::c_int, status: libc::c_int) -> Result<ExitObservation> {
    if si_code == libc::CLD_EXITED {
        return Ok(ExitObservation::Exited(ExitStatus {
            code: Some(status),
            signal: None,
        }));
    }
    if matches!(si_code, libc::CLD_KILLED | libc::CLD_DUMPED) {
        return Ok(ExitObservation::Exited(ExitStatus {
            code: None,
            signal: Some(
                Signal::try_from(status)
                    .map(|signal| format!("{signal:?}"))
                    .unwrap_or_else(|_| format!("signal {status}")),
            ),
        }));
    }
    Err(Error::Runner(format!(
        "waitid returned unexpected child event code {si_code}"
    )))
}

#[cfg(target_vendor = "apple")]
fn waitid_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // Rust libc models Apple's siginfo payload with hidden union storage and
    // exposes it through these unsafe accessors, not public struct fields.
    // Keeping this separate makes each target compile against its own ABI.
    unsafe { info.si_pid() }
}

#[cfg(target_vendor = "apple")]
fn waitid_status(info: &libc::siginfo_t) -> libc::c_int {
    unsafe { info.si_status() }
}

#[cfg(target_os = "linux")]
fn waitid_pid(info: &libc::siginfo_t) -> libc::pid_t {
    unsafe { info.si_pid() }
}

#[cfg(target_os = "linux")]
fn waitid_status(info: &libc::siginfo_t) -> libc::c_int {
    unsafe { info.si_status() }
}

#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn observe_exit_without_reaping(pid: i32) -> Result<ExitObservation> {
    // SAFETY: waitid initializes siginfo for this child (or leaves si_pid at
    // zero with WNOHANG). WNOWAIT is the key lifecycle invariant: the session
    // retains the leader PID/PGID until close has signaled the whole group.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        #[cfg(target_os = "linux")]
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(ExitObservation::ExitedWithoutStatus);
        }
        #[cfg(target_vendor = "apple")]
        if matches!(error.raw_os_error(), Some(libc::EPERM) | Some(libc::ECHILD)) {
            return match observe_exit_with_kqueue(pid) {
                Ok(observation) => Ok(observation),
                Err(kqueue_error) => match observe_proc_state(pid) {
                    Ok(ProcState::Zombie | ProcState::Gone) => {
                        Ok(ExitObservation::ExitedWithoutStatus)
                    }
                    Ok(ProcState::Live) => Ok(ExitObservation::Running),
                    Err(proc_error) => Err(Error::Runner(format!(
                        "could not inspect macOS child state with kqueue ({kqueue_error}) or libproc ({proc_error})"
                    ))),
                },
            };
        }
        return Err(Error::Io(error));
    }
    if waitid_pid(&info) == 0 {
        return Ok(ExitObservation::Running);
    }
    let status = waitid_status(&info);
    decode_waitid_exit(info.si_code, status)
}

#[cfg(all(unix, not(any(target_vendor = "apple", target_os = "linux"))))]
fn observe_exit_without_reaping(_pid: i32) -> Result<ExitObservation> {
    Err(Error::Runner(
        "non-reaping child observation is supported only on macOS and Linux".into(),
    ))
}

fn terminate_spawned_child(child: &mut Box<dyn Child + Send + Sync>, process_group: Option<i32>) {
    #[cfg(unix)]
    if let Some(process_group) = process_group {
        let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = process_group;
    let _ = child.kill();
    // This helper is used only after the child has successfully spawned but
    // before ownership can move into ProcessInner. Reap synchronously so an
    // error constructing the remaining PTY handles cannot leak a child.
    let _ = child.wait();
}

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

#[derive(Clone, Debug)]
enum LeaderLifecycle {
    Running,
    ExitedUnreaped(ExitStatus),
    ExitedUnreapedStatusUnavailable,
    Reaped(ExitStatus),
}

impl LeaderLifecycle {
    fn public_state(&self) -> Result<Option<ProcessState>> {
        match self {
            Self::Running => Ok(None),
            Self::ExitedUnreaped(status) | Self::Reaped(status) => {
                Ok(Some(ProcessState::Exited(status.clone())))
            }
            Self::ExitedUnreapedStatusUnavailable => Ok(Some(ProcessState::Exited(ExitStatus {
                code: None,
                signal: None,
            }))),
        }
    }

    fn record_reaped(&mut self, status: ExitStatus) {
        // Reaping is authoritative even when an earlier non-consuming macOS
        // observation could only establish that the process had exited.
        *self = Self::Reaped(status);
    }
}

struct ProcessInner {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    close_lock: Mutex<()>,
    leader: Mutex<LeaderLifecycle>,
    cleanup_complete: AtomicBool,
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
        validate_dimensions(options.cols, options.rows)?;
        if options.shutdown_timeout.is_zero() {
            return Err(Error::InvalidTimeout {
                field: "shutdown_timeout",
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
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|source| Error::Launch {
                command: command_display.clone(),
                source: source.into(),
            })?;
        #[cfg(unix)]
        let process_group = {
            let child_pid = child.process_id().and_then(|pid| i32::try_from(pid).ok());
            let group_leader = pair.master.process_group_leader();
            match (child_pid, group_leader) {
                (Some(pid), Some(group)) if pid == group => Some(group),
                (pid, group) => {
                    terminate_spawned_child(&mut child, None);
                    return Err(Error::Launch {
                        command: command_display.clone(),
                        source: std::io::Error::other(format!(
                            "PTY child was not isolated as its process-group leader \
                             (pid={pid:?}, pgid={group:?})"
                        ))
                        .into(),
                    });
                }
            }
        };
        #[cfg(not(unix))]
        let process_group = None;
        drop(pair.slave);
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(source) => {
                terminate_spawned_child(&mut child, process_group);
                return Err(Error::Launch {
                    command: command_display.clone(),
                    source: source.into(),
                });
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(source) => {
                terminate_spawned_child(&mut child, process_group);
                return Err(Error::Launch {
                    command: command_display.clone(),
                    source: source.into(),
                });
            }
        };
        let process = Self {
            inner: Arc::new(ProcessInner {
                master: Mutex::new(pair.master),
                writer: Arc::new(Mutex::new(writer)),
                child: Mutex::new(child),
                close_lock: Mutex::new(()),
                leader: Mutex::new(LeaderLifecycle::Running),
                cleanup_complete: AtomicBool::new(false),
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
        let mut leader = self.inner.leader.lock().expect("leader lock poisoned");
        #[cfg(unix)]
        self.reap_observed_exit_if_tree_finished(&mut leader)?;
        if let Some(state) = leader.public_state()? {
            return Ok(state);
        }
        #[cfg(unix)]
        return self.observe_unix_state(&mut leader);
        #[cfg(not(unix))]
        return self.observe_non_unix_state(&mut leader);
    }

    #[cfg(unix)]
    fn observe_unix_state(&self, leader: &mut LeaderLifecycle) -> Result<ProcessState> {
        let observation = {
            let child = self.inner.child.lock().expect("child lock poisoned");
            let pid = child.process_id().and_then(|pid| i32::try_from(pid).ok());
            match pid {
                Some(pid) => observe_exit_without_reaping(pid)?,
                None => ExitObservation::Running,
            }
        };
        match observation {
            ExitObservation::Running => Ok(ProcessState::Running),
            ExitObservation::Exited(status) => {
                *leader = LeaderLifecycle::ExitedUnreaped(status.clone());
                self.inner
                    .events
                    .lock()
                    .expect("events lock poisoned")
                    .push(format!("process exited: {status}"));
                self.reap_observed_exit_if_tree_finished(leader)?;
                leader
                    .public_state()?
                    .ok_or_else(|| Error::Runner("observed exit state was lost".into()))
            }
            #[cfg(any(target_vendor = "apple", target_os = "linux"))]
            ExitObservation::ExitedWithoutStatus => {
                *leader = LeaderLifecycle::ExitedUnreapedStatusUnavailable;
                self.inner
                    .events
                    .lock()
                    .expect("events lock poisoned")
                    .push("process exited without an observable status".into());
                self.reap_observed_exit_if_tree_finished(leader)?;
                leader.public_state()?.ok_or_else(|| {
                    Error::Runner("observed exit-without-status state was lost".into())
                })
            }
        }
    }

    #[cfg(not(unix))]
    fn observe_non_unix_state(&self, leader: &mut LeaderLifecycle) -> Result<ProcessState> {
        let mut child = self.inner.child.lock().expect("child lock poisoned");
        match child.try_wait()? {
            Some(status) => {
                let normalized = normalize_status(status);
                leader.record_reaped(normalized.clone());
                self.inner.cleanup_complete.store(true, Ordering::Release);
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

    #[cfg(unix)]
    fn reap_observed_exit_if_tree_finished(&self, leader: &mut LeaderLifecycle) -> Result<()> {
        if !matches!(
            leader,
            LeaderLifecycle::ExitedUnreaped(_) | LeaderLifecycle::ExitedUnreapedStatusUnavailable
        ) {
            return Ok(());
        }
        let Some(process_group) = self.inner.process_group else {
            return Ok(());
        };
        // Reap only after an authoritative scan proves the group has no live
        // members. An unavailable or inconclusive scan keeps the leader as a
        // zombie temporarily, reserving its numeric PGID for safe cleanup.
        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        {
            let scan_deadline = Instant::now() + Duration::from_millis(10);
            loop {
                match process_group_liveness(process_group) {
                    Ok(GroupLiveness::Empty) => break,
                    Ok(GroupLiveness::Live) => return Ok(()),
                    Ok(GroupLiveness::Inconclusive) => {
                        let remaining = scan_deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Ok(());
                        }
                        thread::sleep(Duration::from_millis(1).min(remaining));
                    }
                    Err(_) => {
                        let remaining = scan_deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Ok(());
                        }
                        thread::sleep(Duration::from_millis(1).min(remaining));
                    }
                }
            }
        }
        #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
        return Ok(());

        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        if let Some(status) = self
            .inner
            .child
            .lock()
            .expect("child lock poisoned")
            .try_wait()?
        {
            let normalized = normalize_status(status);
            leader.record_reaped(normalized);
            self.inner.cleanup_complete.store(true, Ordering::Release);
            self.record_event("process reaped after terminal state observation");
        }
        Ok(())
    }
    pub(crate) fn output_drained(&self) -> bool {
        self.inner.output_drained.load(Ordering::Acquire)
    }
    pub(crate) fn mark_output_drained(&self) {
        self.inner.output_drained.store(true, Ordering::Release);
    }
    pub(crate) fn record_event(&self, event: impl Into<String>) {
        self.inner
            .events
            .lock()
            .expect("events lock poisoned")
            .push(event.into());
    }
    pub fn process_id(&self) -> Option<u32> {
        self.inner
            .child
            .lock()
            .expect("child lock poisoned")
            .process_id()
    }
    #[cfg(unix)]
    pub fn process_group_id(&self) -> Option<i32> {
        self.inner.process_group
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
        validate_dimensions(cols, rows)?;
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
        if self.inner.cleanup_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        // Refresh terminal state before touching the PTY. This also performs
        // the safe immediate reap when no process-group members remain.
        if self.wait_until_tree_exit(Duration::ZERO)? {
            self.inner.cleanup_complete.store(true, Ordering::Release);
            return Ok(());
        }
        // EOT is a session-level graceful shutdown request. Descendants can
        // still be reading the PTY after the process-group leader exits.
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
        if self.wait_until_tree_exit(self.inner.shutdown_timeout / 3)? {
            self.inner.cleanup_complete.store(true, Ordering::Release);
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
                self.inner.cleanup_complete.store(true, Ordering::Release);
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

        // Always reap the owned child after SIGKILL, independent of
        // best-effort descendant enumeration. Then use the remaining budget
        // to confirm that no live process-group members remain.
        let forced_timeout = self.inner.shutdown_timeout / 3;
        if !self.reap_leader_until(forced_timeout)? {
            return Err(Error::Runner(format!(
                "process leader for `{}` did not exit after forced termination",
                self.inner.command
            )));
        }
        #[cfg(unix)]
        // Group verification gets its own bounded window: a slow leader reap
        // must not reduce descendant verification to a near-zero timeout.
        if !self.wait_until_process_group_exit(forced_timeout)? {
            return Err(Error::Runner(format!(
                "process group for `{}` retained live members after forced termination",
                self.inner.command
            )));
        }
        self.inner.cleanup_complete.store(true, Ordering::Release);
        Ok(())
    }

    fn reap_leader_until(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let reaped = {
                // Keep the lifecycle lock across try_wait and the state
                // update. state() uses this same leader -> child order, so it
                // cannot observe Running after the child has been reaped.
                let mut leader = self.inner.leader.lock().expect("leader lock poisoned");
                if matches!(*leader, LeaderLifecycle::Reaped(_)) {
                    return Ok(true);
                }
                let mut child = self.inner.child.lock().expect("child lock poisoned");
                match child.try_wait()? {
                    Some(status) => {
                        leader.record_reaped(normalize_status(status));
                        true
                    }
                    None => false,
                }
            };
            if reaped {
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
    fn wait_until_process_group_exit(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.process_group_is_running()? {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(10).min(remaining));
        }
    }

    fn wait_until_tree_exit(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.inner.cleanup_complete.load(Ordering::Acquire) {
                return Ok(true);
            }
            let leader_exited = !self.is_running()?;
            if self.inner.cleanup_complete.load(Ordering::Acquire) {
                return Ok(true);
            }
            #[cfg(unix)]
            let group_exited = !self.process_group_is_running()?;
            #[cfg(not(unix))]
            let group_exited = true;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if leader_exited && group_exited {
                #[cfg(unix)]
                {
                    if matches!(
                        *self.inner.leader.lock().expect("leader lock poisoned"),
                        LeaderLifecycle::Reaped(_)
                    ) {
                        return Ok(true);
                    }
                    return self.reap_leader_until(remaining);
                }
                #[cfg(not(unix))]
                return Ok(true);
            }
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
            // Darwin can report EPERM when only zombie members remain. Treat
            // that case as empty only after enumerating and inspecting every
            // member; live or uninspectable members keep the error visible.
            #[cfg(target_vendor = "apple")]
            Err(Errno::EPERM) => {
                if matches!(process_group_liveness(process_group)?, GroupLiveness::Empty) {
                    self.record_event(format!(
                        "{signal:?} skipped: process group contains no live members"
                    ));
                    return Ok(());
                }
                Err(Error::Io(std::io::Error::from_raw_os_error(libc::EPERM)))
            }
            Err(error) => Err(Error::Io(std::io::Error::from_raw_os_error(error as i32))),
        }
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    fn enumerated_group_is_running(&self, process_group: i32) -> Result<bool> {
        match process_group_liveness(process_group)? {
            GroupLiveness::Live => Ok(true),
            GroupLiveness::Empty => Ok(false),
            // killpg(0) proved that the group exists. If libproc/procfs cannot
            // determine whether its remaining members are live, keep
            // waiting and ultimately report an explicit bounded verification
            // failure rather than claiming cleanup succeeded.
            GroupLiveness::Inconclusive => Ok(true),
        }
    }

    #[cfg(unix)]
    fn process_group_is_running(&self) -> Result<bool> {
        let Some(process_group) = self.inner.process_group else {
            return Ok(false);
        };
        match killpg(Pid::from_raw(process_group), None::<Signal>) {
            Ok(()) => {
                #[cfg(any(target_vendor = "apple", target_os = "linux"))]
                return self.enumerated_group_is_running(process_group);
                #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
                Ok(true)
            }
            Err(Errno::ESRCH) => Ok(false),
            #[cfg(target_vendor = "apple")]
            Err(Errno::EPERM) => self.enumerated_group_is_running(process_group),
            Err(error) => Err(Error::Io(std::io::Error::from_raw_os_error(error as i32))),
        }
    }
}

fn normalize_status(status: portable_pty::ExitStatus) -> ExitStatus {
    // portable-pty 0.9 represents every status as either an explicit `u32`
    // exit code or a signal. Its std conversion maps an otherwise unavailable,
    // unsuccessful code to 1, so there is no hidden "unknown means success"
    // state to infer here. Preserve values outside our signed public range as
    // unknown rather than truncating them.
    ExitStatus {
        code: status
            .signal()
            .is_none()
            .then(|| i32::try_from(status.exit_code()).ok())
            .flatten(),
        signal: status.signal().map(str::to_owned),
    }
}

#[cfg(unix)]
fn process_group_may_be_running(process_group: i32) -> bool {
    match killpg(Pid::from_raw(process_group), None::<Signal>) {
        Err(Errno::ESRCH) => false,
        Ok(()) => {
            #[cfg(any(target_vendor = "apple", target_os = "linux"))]
            return !matches!(
                process_group_liveness(process_group),
                Ok(GroupLiveness::Empty)
            );
            #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
            true
        }
        #[cfg(target_vendor = "apple")]
        Err(Errno::EPERM) => !matches!(
            process_group_liveness(process_group),
            Ok(GroupLiveness::Empty)
        ),
        Err(_) => true,
    }
}

impl Drop for ProcessInner {
    fn drop(&mut self) {
        if self.cleanup_complete.load(Ordering::Relaxed) {
            return;
        }
        let child = self.child.get_mut().expect("child lock poisoned");
        // Signal while the unreaped leader still reserves this numeric PGID.
        // Reaping first could allow the identifier to be reused; signaling
        // first also terminates descendants when the leader already exited.
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
        }
        let mut leader_exited = matches!(child.try_wait(), Ok(Some(_)));
        if !leader_exited {
            let _ = child.kill();
        }
        let deadline = Instant::now() + self.shutdown_timeout;
        loop {
            leader_exited |= matches!(child.try_wait(), Ok(Some(_)));
            #[cfg(unix)]
            let group_exited = self
                .process_group
                .is_none_or(|group| !process_group_may_be_running(group));
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

#[cfg(test)]
mod option_tests {
    use super::*;

    #[test]
    fn zero_shutdown_timeout_is_rejected_before_spawning() {
        let mut options = PtyOptions::new("command-must-not-be-spawned");
        options.shutdown_timeout = Duration::ZERO;

        assert!(matches!(
            PtyProcess::spawn(options),
            Err(Error::InvalidTimeout {
                field: "shutdown_timeout"
            })
        ));
    }

    #[test]
    fn excessive_dimensions_are_rejected_before_spawning() {
        let mut options = PtyOptions::new("command-must-not-be-spawned");
        options.cols = u16::MAX;
        options.rows = u16::MAX;

        assert!(matches!(
            PtyProcess::spawn(options),
            Err(Error::ScreenTooLarge { .. })
        ));
    }
}

#[cfg(all(test, any(target_vendor = "apple", target_os = "linux")))]
mod unix_tests {
    use super::*;

    #[test]
    fn target_siginfo_accessors_compile_and_read_zeroed_payload() {
        let info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        assert_eq!(waitid_pid(&info), 0);
        assert_eq!(waitid_status(&info), 0);
    }

    #[test]
    fn unexpected_waitid_event_is_an_actionable_error() {
        assert!(matches!(
            decode_waitid_exit(libc::CLD_STOPPED, libc::SIGSTOP),
            Err(Error::Runner(message)) if message.contains("unexpected child event code")
        ));
    }
}

#[cfg(all(test, target_vendor = "apple"))]
mod tests {
    use super::*;

    #[test]
    fn proc_listpgrppids_returns_a_pid_count_not_a_byte_count() {
        let mut pids = [0_i32; 64];
        let buffer_bytes = std::mem::size_of_val(&pids) as libc::c_int;
        // SAFETY: pids is writable for exactly buffer_bytes bytes.
        let pid_count = unsafe {
            libc::proc_listpgrppids(libc::getpgrp(), pids.as_mut_ptr().cast(), buffer_bytes)
        };
        assert!(pid_count > 0);
        let populated = pids.iter().filter(|&&pid| pid > 0).count();
        assert_eq!(pid_count as usize, populated);
        assert_ne!(pid_count as usize, populated * std::mem::size_of::<i32>());
    }

    #[test]
    fn libproc_scan_errors_preserve_empty_vs_inconclusive() {
        assert_eq!(
            libproc_error_liveness(&std::io::Error::from_raw_os_error(libc::ESRCH)),
            GroupLiveness::Empty
        );
        assert_eq!(
            libproc_error_liveness(&std::io::Error::from_raw_os_error(libc::EPERM)),
            GroupLiveness::Inconclusive
        );
    }

    #[test]
    fn kqueue_registration_errors_are_not_treated_as_running() {
        let event = libc::kevent {
            ident: 1,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ERROR,
            fflags: 0,
            data: libc::EPERM as libc::intptr_t,
            udata: std::ptr::null_mut(),
        };
        assert!(matches!(
            decode_kqueue_exit(1, &event),
            Err(Error::Io(error)) if error.raw_os_error() == Some(libc::EPERM)
        ));
    }

    #[test]
    fn unavailable_exit_status_is_exposed_as_an_unknown_exit() {
        assert_eq!(
            LeaderLifecycle::ExitedUnreapedStatusUnavailable
                .public_state()
                .unwrap(),
            Some(ProcessState::Exited(ExitStatus {
                code: None,
                signal: None,
            }))
        );
    }

    #[test]
    fn reaping_replaces_a_previously_unavailable_exit_status() {
        let status = ExitStatus {
            code: Some(17),
            signal: None,
        };
        let mut lifecycle = LeaderLifecycle::ExitedUnreapedStatusUnavailable;
        lifecycle.record_reaped(status.clone());
        assert_eq!(
            lifecycle.public_state().unwrap(),
            Some(ProcessState::Exited(status))
        );
    }
}

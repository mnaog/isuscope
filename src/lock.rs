//! Exclusive operation lock shared by benchmarks and project scripts.
//!
//! The lock is a `lock` file inside the lock directory, held with `flock(LOCK_EX|LOCK_NB)`.
//! The kernel releases it when the holder exits, so a crashed holder never leaves a lock behind
//! and there is no stale-lock reclamation to race over. The `owner` file next to it is written
//! after the lock is taken and only explains who holds it. The lock file itself is never removed:
//! unlinking it would let one process hold the lock on an unlinked file while another takes a
//! newly created one. Processes started while the lock is held receive [`HELD_ENV`] so nested
//! `isuscope lock` calls run without re-locking.

use anyhow::{Context, Result};
use chrono::Local;
use std::{
    env, fmt, fs,
    io::Write,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

pub const HELD_ENV: &str = "ISUSCOPE_LOCK_HELD";
/// Exit status used when another live operation holds the lock (EX_TEMPFAIL).
pub const BUSY_EXIT_CODE: u8 = 75;

#[derive(Debug)]
pub struct LockBusy {
    pub path: PathBuf,
    pub owner: String,
}

impl fmt::Display for LockBusy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "another modifying operation is running: {}",
            self.path.display()
        )?;
        for line in self.owner.lines() {
            write!(formatter, "\n  {line}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LockBusy {}

pub fn held_by_parent() -> bool {
    env::var(HELD_ENV).is_ok_and(|value| value == "1")
}

pub struct OperationLock {
    path: PathBuf,
    /// Held open for the lifetime of the lock: closing it releases the `flock`.
    _file: fs::File,
}

impl OperationLock {
    /// Returns `Ok(None)` when a parent process already holds the lock.
    pub fn acquire(path: &Path, operation: &str) -> Result<Option<Self>> {
        if held_by_parent() {
            return Ok(None);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        fs::create_dir_all(path).with_context(|| format!("cannot create {}", path.display()))?;
        let lock_path = path.join("lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("cannot open {}", lock_path.display()))?;
        // SAFETY: the descriptor stays owned by `file`, and LOCK_NB never blocks.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(error).with_context(|| format!("cannot lock {}", lock_path.display()));
            }
            let owner = fs::read_to_string(path.join("owner")).unwrap_or_default();
            return Err(LockBusy {
                path: path.to_path_buf(),
                owner,
            }
            .into());
        }
        let pid = std::process::id();
        let owner = format!(
            "pid={pid}\nstarted_at={}\noperation={operation}\n",
            Local::now().format("%Y-%m-%dT%H:%M:%S%z")
        );
        if let Err(error) = fs::write(path.join("owner"), owner) {
            return Err(error).with_context(|| format!("cannot write {}/owner", path.display()));
        }
        Ok(Some(Self {
            path: path.to_path_buf(),
            _file: file,
        }))
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        // The `flock` is released when `_file` closes; drop the explanation with it.
        let _ = fs::remove_file(self.path.join("owner"));
    }
}

/// Runs `command` while holding the lock and returns its exit status code.
/// Each locked run appends `started_at, operation, seconds, exit` to `operation-timing.tsv`
/// next to the lock so slow steps can be measured later.
pub fn run_locked(path: &Path, command: &[String]) -> Result<i32> {
    let (program, arguments) = command
        .split_first()
        .context("isuscope lock requires a command after --")?;
    let operation = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_owned();
    let guard = OperationLock::acquire(path, &operation)?;
    let started_at = Local::now();
    let started = Instant::now();
    let mut child = Command::new(program);
    child.args(arguments).env(HELD_ENV, "1");
    // Ctrl-C reaches the whole foreground process group; wait for the child to finish its
    // own cleanup and release the lock afterwards instead of dying first.
    let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            child.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
    }
    let status = child.status();
    unsafe { libc::signal(libc::SIGINT, previous) };
    let status = status.with_context(|| format!("cannot start {program}"))?;
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    });
    if guard.is_some() {
        let timing = path.with_file_name("operation-timing.tsv");
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&timing)
        {
            let _ = writeln!(
                file,
                "{}\t{operation}\t{:.1}\t{code}",
                started_at.format("%Y-%m-%dT%H:%M:%S%z"),
                started.elapsed().as_secs_f64()
            );
        }
    }
    drop(guard);
    Ok(code)
}

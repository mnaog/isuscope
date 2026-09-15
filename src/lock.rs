//! Exclusive operation lock shared by benchmarks and project scripts.
//!
//! The lock is a directory created atomically with `mkdir`, holding an `owner` file with the
//! holder's pid. A lock whose pid no longer exists is reclaimed. Processes started while the
//! lock is held receive [`HELD_ENV`] so nested `isuscope lock` calls run without re-locking.

use anyhow::{Context, Result};
use chrono::Local;
use std::{
    env, fmt, fs,
    io::Write,
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
    pid: u32,
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
        if fs::create_dir(path).is_err() {
            let owner = fs::read_to_string(path.join("owner")).unwrap_or_default();
            let stale = owner_pid(&owner).is_some_and(|pid| !process_alive(pid));
            if stale {
                let _ = fs::remove_file(path.join("owner"));
                let _ = fs::remove_dir(path);
            }
            if fs::create_dir(path).is_err() {
                let owner = fs::read_to_string(path.join("owner")).unwrap_or_default();
                return Err(LockBusy {
                    path: path.to_path_buf(),
                    owner,
                }
                .into());
            }
        }
        let pid = std::process::id();
        let owner = format!(
            "pid={pid}\nstarted_at={}\noperation={operation}\n",
            Local::now().format("%Y-%m-%dT%H:%M:%S%z")
        );
        if let Err(error) = fs::write(path.join("owner"), owner) {
            let _ = fs::remove_dir(path);
            return Err(error).with_context(|| format!("cannot write {}/owner", path.display()));
        }
        Ok(Some(Self {
            path: path.to_path_buf(),
            pid,
        }))
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let owner = fs::read_to_string(self.path.join("owner")).unwrap_or_default();
        if owner_pid(&owner) == Some(self.pid) {
            let _ = fs::remove_file(self.path.join("owner"));
            let _ = fs::remove_dir(&self.path);
        }
    }
}

fn owner_pid(owner: &str) -> Option<u32> {
    owner
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|value| value.trim().parse().ok())
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence and permission.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_pid_is_parsed_from_the_owner_file() {
        assert_eq!(owner_pid("pid=42\nstarted_at=x\n"), Some(42));
        assert_eq!(owner_pid("operation=x\n"), None);
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(99_999_999));
    }
}

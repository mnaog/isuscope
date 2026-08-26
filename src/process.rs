use std::{io, process::ExitStatus, time::Duration};
use tokio::process::{Child, Command};

pub fn configure_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

pub async fn terminate_group(child: &mut Child) -> io::Result<ExitStatus> {
    #[cfg(unix)]
    {
        if let Some(id) = child.id() {
            // SAFETY: a negative PID targets only the process group created for this child.
            unsafe {
                libc::kill(-(id as i32), libc::SIGTERM);
            }
        }
        if let Ok(status) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            return status;
        }
        if let Some(id) = child.id() {
            // SAFETY: the process group ID is derived from the still-running child.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
        child.wait().await
    }
    #[cfg(not(unix))]
    {
        child.kill().await?;
        child.wait().await
    }
}

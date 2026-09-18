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
        let group = child.id().map(|id| -(id as i32));
        if let Some(group) = group {
            // SAFETY: a negative PID targets only the process group created for this child.
            unsafe {
                libc::kill(group, libc::SIGTERM);
            }
        }
        let waited = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        if let Some(group) = group {
            // 直接の子が終わっても、同じprocess groupの孫が動いたままstdout/stderrのpipeを
            // 握り続けることがある。それを待つと中断が返らないので、group全体を必ず落とす。
            // SAFETY: the process group ID is derived from the child this function owns.
            unsafe {
                libc::kill(group, libc::SIGKILL);
            }
        }
        match waited {
            Ok(status) => status,
            Err(_) => child.wait().await,
        }
    }
    #[cfg(not(unix))]
    {
        child.kill().await?;
        child.wait().await
    }
}

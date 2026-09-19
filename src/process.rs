use std::{io, process::ExitStatus, time::Duration};
use tokio::process::{Child, Command};

/// 子processが終わってから、stdoutとstderrの取り込みが終わるのを待つ締切。
pub const CAPTURE_DEADLINE: Duration = Duration::from_secs(10);

/// 取り込みtaskの終わり方。
pub enum Capture<T> {
    Finished(Result<T, tokio::task::JoinError>),
    /// 締切までにpipeが閉じなかった（pipeを握った孫processが残っている）。taskは止め、
    /// 止まったことまで確かめてある。そこまでに書けた分だけがlogに残っている。
    Abandoned,
}

/// 子processが終わった後の取り込みtaskを、締切まで待つ。直接の子が終わっても、同じprocess
/// groupの孫がpipeを握っているとEOFが来ず、待ち続けてしまう。締切を過ぎたら、`group`
/// （子のprocess group）があればそれを落とし、taskを止める。JoinHandleをdropしてもtaskは止まらず、
/// `abort()`も呼んだ時点では止まっていないので、joinまで待つ。
pub async fn finish_capture<T>(
    mut task: tokio::task::JoinHandle<T>,
    deadline: Duration,
    group: Option<u32>,
) -> Capture<T> {
    match tokio::time::timeout(deadline, &mut task).await {
        Ok(joined) => Capture::Finished(joined),
        Err(_) => {
            #[cfg(unix)]
            if let Some(group) = group {
                // SAFETY: a negative PID targets only the process group created for this child.
                unsafe {
                    libc::kill(-(group as i32), libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            let _ = group;
            task.abort();
            let _ = task.await;
            Capture::Abandoned
        }
    }
}

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

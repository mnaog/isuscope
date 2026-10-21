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

/// 子processが終わった後の取り込みtask（stdoutとstderrなど）を、1つの締切まで待つ。直接の子が
/// 終わっても、同じprocess groupの孫がpipeを握っているとEOFが来ず、待ち続けてしまう。締切は
/// 全taskで共有するので、待つのは最長でも`deadline`1回分。過ぎたら、`group`（子のprocess group）
/// があればそれを落とし、残ったtaskを止める。JoinHandleをdropしてもtaskは止まらず、`abort()`も
/// 呼んだ時点では止まっていないので、joinまで待つ。結果は渡した順に返す。
pub async fn finish_captures<T, const N: usize>(
    tasks: [tokio::task::JoinHandle<T>; N],
    deadline: Duration,
    group: Option<u32>,
) -> [Capture<T>; N] {
    let until = tokio::time::Instant::now() + deadline;
    let mut waited = Vec::with_capacity(N);
    for mut task in tasks {
        // The task is polled before the deadline is checked, so one that already finished
        // is taken even after an earlier task used up the time.
        match tokio::time::timeout_at(until, &mut task).await {
            Ok(joined) => waited.push(Ok(joined)),
            Err(_) => waited.push(Err(task)),
        }
    }
    if waited.iter().any(Result::is_err) {
        #[cfg(unix)]
        if let Some(group) = group {
            // SAFETY: a negative PID targets only the process group created for this child.
            unsafe {
                libc::kill(-(group as i32), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = group;
    }
    let mut captures = Vec::with_capacity(N);
    for result in waited {
        captures.push(match result {
            Ok(joined) => Capture::Finished(joined),
            Err(task) => {
                task.abort();
                let _ = task.await;
                Capture::Abandoned
            }
        });
    }
    captures
        .try_into()
        .unwrap_or_else(|_| unreachable!("one capture per task"))
}

/// Process groups of children that are running now, so an immediate exit can stop them
/// (each child gets its own group, which a terminal's signal does not reach).
static GROUPS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Keeps a child's process group in the registry until dropped.
pub struct RegisteredGroup(Option<u32>);

/// Registers the process group of a child started with [`configure_group`].
pub fn register_group(group: Option<u32>) -> RegisteredGroup {
    if let (Some(group), Ok(mut groups)) = (group, GROUPS.lock()) {
        groups.push(group);
    }
    RegisteredGroup(group)
}

impl Drop for RegisteredGroup {
    fn drop(&mut self) {
        if let (Some(group), Ok(mut groups)) = (self.0, GROUPS.lock())
            && let Some(position) = groups.iter().position(|value| *value == group)
        {
            groups.swap_remove(position);
        }
    }
}

/// Sends SIGKILL to every registered process group. Used just before exiting on a second signal.
pub fn kill_registered_groups() {
    #[cfg(unix)]
    if let Ok(groups) = GROUPS.lock() {
        for group in groups.iter() {
            // SAFETY: a negative PID targets only a process group created for one of our children.
            unsafe {
                libc::kill(-(*group as i32), libc::SIGKILL);
            }
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

//! Free space on the benchmark nodes. Logs that grow with every benchmark fill a node's disk
//! quietly; deploys and databases then fail on a node that the local `doctor` check never saw.
use crate::config::{DiskConfig, LoadedConfig};
use anyhow::{Context, Result};
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountFree {
    pub mount: String,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskLevel {
    Ok,
    Low,
    TooLow,
}

#[derive(Debug)]
pub struct NodeDisk {
    pub node: String,
    pub target: String,
    /// Mounts backing the configured paths, or why they could not be measured.
    pub mounts: Result<Vec<MountFree>>,
}

impl NodeDisk {
    /// The emptiest mount and how it compares with the thresholds.
    pub fn tightest(&self, disk: &DiskConfig) -> Option<(&MountFree, DiskLevel)> {
        let mounts = self.mounts.as_ref().ok()?;
        let mount = mounts.iter().min_by_key(|mount| mount.free_bytes)?;
        Some((mount, level(mount.free_bytes, disk)))
    }
}

pub fn level(free_bytes: u64, disk: &DiskConfig) -> DiskLevel {
    let mib = free_bytes / (1024 * 1024);
    if mib < disk.node_min_free_mb {
        DiskLevel::TooLow
    } else if mib < disk.node_warn_free_mb {
        DiskLevel::Low
    } else {
        DiskLevel::Ok
    }
}

pub fn describe(mount: &MountFree) -> String {
    let mib = mount.free_bytes / (1024 * 1024);
    if mib >= 1024 {
        format!("{} has {:.1} GiB free", mount.mount, mib as f64 / 1024.0)
    } else {
        format!("{} has {mib} MiB free", mount.mount)
    }
}

/// Measures every configured node in parallel with `df -Pk`, in node order.
pub async fn measure(config: &LoadedConfig) -> Vec<NodeDisk> {
    let disk = &config.config.disk;
    if disk.paths.is_empty() {
        return Vec::new();
    }
    let timeout = Duration::from_secs(config.config.ssh.connect_timeout_seconds + 5);
    let mut checks = tokio::task::JoinSet::new();
    for (index, node) in config
        .config
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| !node.rule_side)
    {
        let user = node.user.as_deref().unwrap_or(&config.config.ssh.user);
        let target = format!("{user}@{}", node.host);
        let mut args = config.ssh_options();
        let paths = disk
            .paths
            .iter()
            .map(|path| format!("'{}'", path.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        args.extend([target.clone(), "--".into(), format!("df -Pk {paths}")]);
        let name = node.name.clone();
        checks.spawn(async move {
            let output =
                tokio::time::timeout(timeout, Command::new("ssh").args(&args).output()).await;
            let mounts = match output {
                // df exits non-zero when one path is missing but still reports the others.
                Ok(Ok(output)) => {
                    let mounts = parse_df(&String::from_utf8_lossy(&output.stdout));
                    if mounts.is_empty() {
                        Err(anyhow::anyhow!(
                            "df reported nothing: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        ))
                    } else {
                        Ok(mounts)
                    }
                }
                Ok(Err(error)) => Err(error).context("cannot start ssh"),
                Err(_) => Err(anyhow::anyhow!("ssh timed out")),
            };
            (
                index,
                NodeDisk {
                    node: name,
                    target,
                    mounts,
                },
            )
        });
    }
    let mut results = checks.join_all().await.into_iter().collect::<Vec<_>>();
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, disk)| disk).collect()
}

/// Parses POSIX `df -Pk` output into one entry per mount point.
pub fn parse_df(output: &str) -> Vec<MountFree> {
    let mut mounts = Vec::<MountFree>::new();
    for line in output.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 6 {
            continue;
        }
        let Ok(available_kib) = fields[3].parse::<u64>() else {
            continue;
        };
        let mount = fields[5..].join(" ");
        if mounts.iter().all(|existing| existing.mount != mount) {
            mounts.push(MountFree {
                mount,
                free_bytes: available_kib * 1024,
            });
        }
    }
    mounts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk() -> DiskConfig {
        DiskConfig::default()
    }

    #[test]
    fn parses_posix_df_and_deduplicates_mounts() {
        let output = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                      /dev/root         30298176 26000000   3072000      90% /\n\
                      /dev/root         30298176 26000000   3072000      90% /\n\
                      tmpfs              1876260        0   1876260       0% /run/user data\n";
        assert_eq!(
            parse_df(output),
            vec![
                MountFree {
                    mount: "/".into(),
                    free_bytes: 3_072_000 * 1024
                },
                MountFree {
                    mount: "/run/user data".into(),
                    free_bytes: 1_876_260 * 1024
                },
            ]
        );
    }

    #[test]
    fn classifies_free_space_against_thresholds() {
        let mib = 1024 * 1024;
        assert_eq!(level(500 * mib, &disk()), DiskLevel::TooLow);
        assert_eq!(level(2048 * mib, &disk()), DiskLevel::Low);
        assert_eq!(level(8192 * mib, &disk()), DiskLevel::Ok);
    }
}

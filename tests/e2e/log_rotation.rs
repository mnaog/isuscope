use super::*;

#[cfg(unix)]
#[test]
fn standard_log_delta_survives_common_rotation_strategies() {
    // The shipped template carries placeholders; render it the way `init` does.
    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let collectors = config["collectors"].as_array().unwrap();
    let script = |name: &str| {
        collectors
            .iter()
            .find(|collector| collector["name"].as_str() == Some(name))
            .unwrap()["command"][2]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let mark_template = script("nginx-log-mark");
    let delta_template = script("nginx-log-delta");

    // Collectors run on Linux nodes. Adapt their GNU utility calls to the native
    // macOS tools for local tests, retaining real file identities and timestamps.
    #[cfg(target_os = "macos")]
    let utilities = {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        for (name, script) in [
            (
                "stat",
                r#"#!/bin/sh
set -eu
test "$#" -eq 3 && test "$1" = -c || exit 1
case "$2" in
  '%d %i') exec /usr/bin/stat -f '%d %i' "$3" ;;
  '%Y') exec /usr/bin/stat -f '%m' "$3" ;;
  *) exit 1 ;;
esac
"#,
            ),
            (
                "sha256sum",
                "#!/bin/sh\nexec /usr/bin/shasum -a 256 \"$@\"\n",
            ),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        directory
    };
    let shell = || {
        let mut command = Command::new("sh");
        command.arg("-c");
        #[cfg(target_os = "macos")]
        {
            let mut paths = vec![utilities.path().to_path_buf()];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            command.env("PATH", std::env::join_paths(paths).unwrap());
        }
        command
    };

    let run_case = |name: &str, initial: &[u8], rotate: &dyn Fn(&std::path::Path)| {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("access.log");
        fs::write(&log, initial).unwrap();
        let prefix = directory.path().join(format!("isuscope-{name}"));
        let prepare = |template: &str| {
            template
                .replace("/var/log/nginx/access.log", log.to_str().unwrap())
                .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
        };
        let mark = shell().arg(prepare(&mark_template)).output().unwrap();
        assert!(
            mark.status.success(),
            "mark failed for {name}: {}",
            String::from_utf8_lossy(&mark.stderr)
        );
        rotate(&log);
        let delta = shell().arg(prepare(&delta_template)).output().unwrap();
        assert!(
            delta.status.success(),
            "delta failed for {name}: {}",
            String::from_utf8_lossy(&delta.stderr)
        );
        // 差分はnodeに残し、後のalpが集計する。stdoutには大きさだけが出る。
        let kept = fs::read(format!("{}.nginx.log", prefix.display())).unwrap();
        assert!(
            String::from_utf8_lossy(&delta.stdout).contains(&format!(
                "\"name\":\"http.access_log_bytes\",\"value\":{}",
                kept.len()
            )),
            "{}",
            String::from_utf8_lossy(&delta.stdout)
        );
        kept
    };

    assert_eq!(
        run_case("append", b"before\n", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"appended\n")
                .unwrap();
        }),
        b"appended\n"
    );
    assert_eq!(
        run_case("rename", b"before\n", &|log| {
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"new\n").unwrap();
        }),
        b"new\n"
    );
    assert_eq!(
        run_case("copytruncate", b"before\n", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"old-tail\n")
                .unwrap();
            fs::copy(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"new\n").unwrap();
        }),
        b"old-tail\nnew\n"
    );
    assert_eq!(
        run_case("gzip", b"before\n", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"old-tail\n")
                .unwrap();
            let rotated = format!("{}.1", log.display());
            fs::rename(log, &rotated).unwrap();
            assert!(
                Command::new("gzip")
                    .arg(&rotated)
                    .status()
                    .unwrap()
                    .success()
            );
            fs::write(log, b"new\n").unwrap();
        }),
        b"old-tail\nnew\n"
    );
    assert_eq!(
        run_case("multiple", b"before\n", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"first-tail\n")
                .unwrap();
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"second\n").unwrap();
            fs::rename(
                format!("{}.1", log.display()),
                format!("{}.2", log.display()),
            )
            .unwrap();
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"third\n").unwrap();
        }),
        b"first-tail\nsecond\nthird\n"
    );
    assert_eq!(
        run_case("empty-rename", b"", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"first\n")
                .unwrap();
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"second\n").unwrap();
        }),
        b"first\nsecond\n"
    );
}

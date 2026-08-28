use super::*;

#[cfg(unix)]
#[test]
fn standard_log_delta_survives_common_rotation_strategies() {
    let config: toml::Value = toml::from_str(include_str!("../../templates/config.toml")).unwrap();
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

    let run_case = |name: &str, rotate: &dyn Fn(&std::path::Path)| {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("access.log");
        fs::write(&log, b"before\n").unwrap();
        let prefix = directory.path().join(format!("isuscope-{name}"));
        let prepare = |template: &str| {
            template
                .replace("/var/log/nginx/access.log", log.to_str().unwrap())
                .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
        };
        let mark = Command::new("sh")
            .args(["-c", &prepare(&mark_template)])
            .output()
            .unwrap();
        assert!(
            mark.status.success(),
            "mark failed for {name}: {}",
            String::from_utf8_lossy(&mark.stderr)
        );
        rotate(&log);
        let delta = Command::new("sh")
            .args(["-c", &prepare(&delta_template)])
            .output()
            .unwrap();
        assert!(
            delta.status.success(),
            "delta failed for {name}: {}",
            String::from_utf8_lossy(&delta.stderr)
        );
        delta.stdout
    };

    assert_eq!(
        run_case("append", &|log| {
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
        run_case("rename", &|log| {
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"new\n").unwrap();
        }),
        b"new\n"
    );
    assert_eq!(
        run_case("copytruncate", &|log| {
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
        run_case("gzip", &|log| {
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
        run_case("multiple", &|log| {
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
}

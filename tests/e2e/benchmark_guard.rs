use super::*;

fn isuscope(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .env("SCAFFOLD_SETTING", "from-the-shell")
        .output()
        .unwrap()
}

fn latest_manifest(project: &std::path::Path) -> serde_json::Value {
    let list: serde_json::Value = parsed(&isuscope(project, &["list"]).stdout).unwrap();
    let runs = project.join(".isuscope/runs");
    let id = full_run_id(&runs, list["runs"]["rows"][0]["run"].as_str().unwrap());
    parsed(&fs::read(runs.join(id).join("run.json")).unwrap()).unwrap()
}

/// A passing run must be analyzed before the next benchmark starts.
fn analyze_latest(project: &std::path::Path) {
    let analysis = isuscope(
        project,
        &[
            "analyze",
            "latest",
            "inconclusive",
            "--analysis",
            "test run",
        ],
    );
    assert!(
        analysis.status.success(),
        "{}",
        String::from_utf8_lossy(&analysis.stderr)
    );
}

const RESULT: &str = r#"printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":1,\"pass\":true}'"#;

#[cfg(unix)]
#[test]
fn the_same_deployment_is_not_measured_twice_without_a_reason() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let deployed = project.path().join("deployed");
    let prepared = project.path().join("prepared.log");
    fs::write(&deployed, r#"{"commit":"abc123","deployed":true}"#).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "{RESULT}"]
identity_command = ["cat", "{deployed}"]
prepare_command = ["sh", "-c", "printf '%s|%s\n' \"${{ISUSCOPE_REPEAT_REASON:-}}\" \"$SCAFFOLD_SETTING\" >> {prepared}"]
"#,
            deployed = deployed.display(),
            prepared = prepared.display(),
        ),
    )
    .unwrap();

    let first = isuscope(project.path(), &["run", "--hypothesis", "base"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(
        stdout.contains("identity  commit=abc123 deployed=true"),
        "{stdout}"
    );
    analyze_latest(project.path());

    // Nothing was deployed in between: the benchmark does not start, and without before
    // collectors it is refused before prepare_command. A refusal is not a result: no run is
    // saved.
    let first_manifest = latest_manifest(project.path());
    let refused = isuscope(project.path(), &["run", "--hypothesis", "same again"]);
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--repeat"), "{stderr}");
    let stdout = String::from_utf8_lossy(&refused.stdout);
    assert!(stdout.contains("not saved"), "{stdout}");
    assert_eq!(latest_manifest(project.path())["id"], first_manifest["id"]);
    let list: serde_json::Value = parsed(&isuscope(project.path(), &["list"]).stdout).unwrap();
    assert_eq!(list["runs"]["rows"].as_array().unwrap().len(), 1);
    let incomplete = fs::read_dir(config_dir.join("runs/.incomplete"))
        .unwrap()
        .count();
    assert_eq!(incomplete, 0);

    // A stated reason measures it again, and prepare sees the reason and the shell's settings.
    let repeated = isuscope(
        project.path(),
        &[
            "run",
            "--hypothesis",
            "variance",
            "--repeat",
            "check the variance",
        ],
    );
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    analyze_latest(project.path());
    let manifest = latest_manifest(project.path());
    assert_eq!(manifest["repeat"]["reason"], "check the variance");
    // The refused run never started a benchmark, so the base is still the first run.
    let first_id = manifest["repeat"]["base_run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first_id, manifest["id"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(&prepared).unwrap(),
        "|from-the-shell\ncheck the variance|from-the-shell\n"
    );

    // A new deployment is a new measurement, and the run names what changed.
    fs::write(&deployed, r#"{"commit":"def456","deployed":true}"#).unwrap();
    let changed = isuscope(project.path(), &["run", "--hypothesis", "new commit"]);
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let stdout = String::from_utf8_lossy(&changed.stdout);
    assert!(stdout.contains("changed   identity.commit"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn a_repeat_found_by_before_collectors_is_not_saved_and_skips_after_collectors() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let after = project.path().join("after.log");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "{RESULT}"]
identity_command = ["echo", "{{\"commit\":\"abc\"}}"]

[[collectors]]
name = "binary"
phase = "before"
command = ["sh", "-c", "echo '{{\"type\":\"fingerprint\",\"name\":\"app.binary\",\"value\":\"v1\"}}'"]

[[collectors]]
name = "after"
phase = "after"
command = ["sh", "-c", "echo ran >> {after}"]
"#,
            after = after.display(),
        ),
    )
    .unwrap();
    let first = isuscope(project.path(), &["run", "--hypothesis", "base"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    analyze_latest(project.path());
    let refused = isuscope(project.path(), &["run", "--hypothesis", "same again"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--repeat"));
    assert_eq!(fs::read_to_string(&after).unwrap(), "ran\n");
    let list: serde_json::Value = parsed(&isuscope(project.path(), &["list"]).stdout).unwrap();
    assert_eq!(list["runs"]["rows"].as_array().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn a_stopped_benchmark_is_not_the_base_of_the_repeat_check() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let hang = project.path().join("hang");
    fs::write(&hang, "").unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "if [ -e {hang} ]; then sleep 30; fi; {RESULT}"]
idle_timeout_seconds = 1
identity_command = ["echo", "{{\"commit\":\"abc\"}}"]
"#,
            hang = hang.display(),
        ),
    )
    .unwrap();
    let stopped = isuscope(project.path(), &["run", "--hypothesis", "hangs"]);
    assert!(!stopped.status.success());
    assert_eq!(
        latest_manifest(project.path())["benchmark"]["stopped_by"],
        "idle"
    );
    // The same deployment again: the stopped run measured nothing, so this one is not refused.
    fs::remove_file(&hang).unwrap();
    let measured = isuscope(project.path(), &["run", "--hypothesis", "measure it"]);
    assert!(
        measured.status.success(),
        "{}",
        String::from_utf8_lossy(&measured.stderr)
    );
}

#[cfg(unix)]
#[test]
fn a_failed_prepare_records_a_failed_run_without_a_benchmark() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let benchmarked = project.path().join("benchmarked");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch {benchmarked}; {RESULT}"]
prepare_command = ["sh", "-c", "echo 'deployed commit differs from HEAD' >&2; exit 3"]

[[collectors]]
name = "never"
phase = "before"
command = ["sh", "-c", "touch {benchmarked}"]
"#,
            benchmarked = benchmarked.display(),
        ),
    )
    .unwrap();
    let run = isuscope(project.path(), &["run", "--hypothesis", "prepare fails"]);
    assert!(!run.status.success());
    assert!(!benchmarked.exists(), "the benchmark or a collector ran");
    let manifest = latest_manifest(project.path());
    assert_eq!(manifest["benchmark"]["mode"], "not-started");
    let error = manifest["benchmark"]["error"].as_str().unwrap();
    assert!(error.contains("prepare_command failed"), "{error}");
    assert!(
        error.contains("deployed commit differs from HEAD"),
        "{error}"
    );
    assert_eq!(manifest["hooks"][0]["name"], "prepare");
    assert_eq!(manifest["hooks"][0]["exit_code"], 3);
}

#[cfg(unix)]
#[test]
fn a_silent_benchmark_is_stopped_and_cleaned_up() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let seen = project.path().join("timeout-seen");
    let cleaned = project.path().join("cleaned");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "echo \"$ISUSCOPE_BENCHMARK_TIMEOUT_SECONDS\" > {seen}; echo started; sleep 30"]
timeout_seconds = 600
idle_timeout_seconds = 1
cleanup_command = ["sh", "-c", "echo \"$ISUSCOPE_RUN_ID\" > {cleaned}"]
"#,
            seen = seen.display(),
            cleaned = cleaned.display(),
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let run = isuscope(project.path(), &["run", "--hypothesis", "the bench hangs"]);
    assert!(!run.status.success());
    assert!(
        started.elapsed().as_secs() < 15,
        "idle timeout did not stop the benchmark"
    );
    // The adapter gets the limit; isuscope's own stop waits a further grace period.
    assert_eq!(fs::read_to_string(&seen).unwrap().trim(), "600");
    let manifest = latest_manifest(project.path());
    assert_eq!(manifest["benchmark"]["stopped_by"], "idle");
    assert_eq!(manifest["benchmark"]["passed"], false);
    assert_eq!(
        fs::read_to_string(&cleaned).unwrap().trim(),
        manifest["id"].as_str().unwrap()
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("stopped   idle"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn a_boundary_collector_supplies_the_initialize_end() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "sleep 1; {RESULT}"]

[[collectors]]
name = "initialize-boundary"
phase = "boundary"
command = ["sh", "-c", "awk -v begin={{benchmark_started_at}} 'BEGIN {{ printf \"{{\\\"type\\\":\\\"isuscope.event\\\",\\\"name\\\":\\\"initialize\\\",\\\"started_at\\\":%.6f,\\\"finished_at\\\":%.6f}}\\n\", begin + 0.1, begin + 0.5 }}'"]
"#
        ),
    )
    .unwrap();
    let run = isuscope(
        project.path(),
        &["run", "--hypothesis", "boundary from the log"],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let manifest = latest_manifest(project.path());
    let benchmark = &manifest["benchmark"];
    assert_eq!(benchmark["initialize_source"], "access_log");
    let at = |field: &str| {
        chrono::DateTime::parse_from_rfc3339(benchmark[field].as_str().unwrap())
            .unwrap()
            .timestamp_micros()
    };
    assert_eq!(at("initialize_finished_at") - at("started_at"), 500_000);
    assert_eq!(at("initialize_started_at") - at("started_at"), 100_000);
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("initialize access_log"), "{stdout}");

    let brief: serde_json::Value =
        parsed(&isuscope(project.path(), &["brief", "latest"]).stdout).unwrap();
    assert!(
        brief["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|warning| !warning.as_str().unwrap().contains("initialize boundary")),
        "{brief}"
    );
}

#[cfg(unix)]
#[test]
fn a_run_without_a_boundary_is_flagged_in_brief() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!("[benchmark]\nmode = \"command\"\ncommand = [\"sh\", \"-c\", \"{RESULT}\"]\n"),
    )
    .unwrap();
    let run = isuscope(project.path(), &["run", "--hypothesis", "no boundary"]);
    assert!(run.status.success());
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("initialize unknown"), "{stdout}");
    let brief: serde_json::Value =
        parsed(&isuscope(project.path(), &["brief", "latest"]).stdout).unwrap();
    assert!(
        brief["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("no initialize boundary")),
        "{brief}"
    );
}

/// The template boundary collector against a real access log delta.
#[cfg(unix)]
#[test]
fn template_boundary_collector_reads_the_last_initialize_in_the_benchmark() {
    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let collector = config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|collector| collector["name"].as_str() == Some("initialize-boundary"))
        .unwrap();
    assert_eq!(collector["phase"].as_str(), Some("boundary"));
    let template = collector["command"][2].as_str().unwrap().to_owned();
    let directory = tempdir().unwrap();
    let log = directory.path().join("access.log");
    let earlier = "method:GET\turi:/old\tmsec:1789653600.000\treqtime:0.010\n";
    fs::write(
        &log,
        [
            earlier,
            "method:POST\turi:/api/initialize\tmsec:1789653661.000\treqtime:0.500\n",
            "method:POST\turi:/api/initialize?x=1\tmsec:1789653663.250\treqtime:1.250\n",
            "method:GET\turi:/api/initialize\tmsec:1789653664.000\treqtime:0.010\n",
            "method:POST\turi:/api/initialize\tmsec:1789653690.000\treqtime:0.010\n",
        ]
        .concat(),
    )
    .unwrap();
    let prefix = directory.path().join("isuscope-t");
    fs::write(
        format!("{}.nginx.marker", prefix.display()),
        format!("{} 0 x 1 2 3\n", earlier.len()),
    )
    .unwrap();
    let script = template
        .replace("/var/log/nginx/access.log", log.to_str().unwrap())
        .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
        .replace("{initialize_method}", "POST")
        .replace("{initialize_path}", "/api/initialize")
        .replace("{benchmark_started_at}", "1789653660.000000")
        .replace("{benchmark_finished_at}", "1789653670.000000");
    let output = Command::new("sh").args(["-c", &script]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // The last POST to the path within the benchmark, query string ignored.
    assert_eq!(event["finished_at"].as_f64().unwrap(), 1789653663.25);
    assert_eq!(event["started_at"].as_f64().unwrap(), 1789653662.0);

    // Without `[initialize]` the collector reports itself unavailable.
    let unset = template
        .replace("{initialize_method}", "")
        .replace("{initialize_path}", "");
    let output = Command::new("sh").args(["-c", &unset]).output().unwrap();
    assert_eq!(output.status.code(), Some(75));
}

#[cfg(unix)]
#[test]
fn a_hook_whose_background_process_keeps_the_pipe_open_still_succeeds() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "{RESULT}"]
prepare_command = ["sh", "-c", "sleep 60 & echo restarted"]
"#
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let run = isuscope(
        project.path(),
        &["run", "--hypothesis", "prepare leaves a child"],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    // One shared capture deadline (10s), not one per stream.
    assert!(started.elapsed().as_secs() < 18, "{:?}", started.elapsed());
    let manifest = latest_manifest(project.path());
    let prepare = &manifest["hooks"][0];
    assert_eq!(prepare["status"], "complete");
    assert!(
        prepare["warnings"][0]
            .as_str()
            .unwrap()
            .contains("stayed open"),
        "{prepare}"
    );
}

#[cfg(unix)]
fn interrupt(child: &std::process::Child) {
    // SAFETY: the PID is that of a child this test started and has not waited for yet.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
}

#[cfg(unix)]
#[test]
fn a_signal_stops_a_waiting_hook_and_a_second_one_exits_at_once() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let started_file = project.path().join("prepare-started");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "{RESULT}"]
prepare_command = ["sh", "-c", "touch {started}; sleep 60"]
"#,
            started = started_file.display(),
        ),
    )
    .unwrap();
    let wait_for = |path: &std::path::Path| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    // One signal: prepare stops at once and the run is saved as aborted.
    let mut child = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "interrupted in prepare"])
        .current_dir(project.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&started_file);
    let interrupted_at = std::time::Instant::now();
    interrupt(&child);
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(
        interrupted_at.elapsed().as_secs() < 8,
        "{:?}",
        interrupted_at.elapsed()
    );
    let manifest = latest_manifest(project.path());
    assert_eq!(manifest["state"], "aborted");
    assert_eq!(manifest["benchmark"]["interrupted"], true);

    // A cleanup after an interrupted benchmark is not cut by that signal, but a second one
    // exits immediately and stops it.
    let cleanup_pid = project.path().join("cleanup.pid");
    let bench_started = project.path().join("bench-started");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch {bench}; sleep 60"]
cleanup_command = ["sh", "-c", "echo $$ > {pid}; exec sleep 60"]
"#,
            bench = bench_started.display(),
            pid = cleanup_pid.display(),
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "interrupted twice"])
        .current_dir(project.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&bench_started);
    interrupt(&child);
    wait_for(&cleanup_pid);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let second_at = std::time::Instant::now();
    interrupt(&child);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    assert!(
        second_at.elapsed().as_secs() < 3,
        "{:?}",
        second_at.elapsed()
    );
    let pid: i32 = fs::read_to_string(&cleanup_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Killed: gone, or a zombie waiting for a reaper that is not ours.
    let alive = fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .next()?
                .to_owned();
            Some(state != "Z" && state != "X")
        })
        .unwrap_or(false);
    assert!(!alive, "the cleanup process survived the exit");
    // The unsaved run is left for the next `run` to recover as aborted.
    assert!(
        fs::read_dir(config_dir.join("runs/.incomplete"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().join("run.json").is_file())
    );
}

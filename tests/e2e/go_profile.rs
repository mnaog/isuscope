use super::*;

/// Goのアプリの窓口が返すprofile（`go-profile-stop`の出力）はparser `go-profile`でrun全体の集計に
/// なり、briefの`go_cpu`・`go_cpu_app`・`go_routes`・`go_classes`・`go_waits`・`go_allocs`欄に出る。
/// アプリ計測のGoのruntimeの値（`app.go.*`）は`app_runtime`欄に負荷区間で要約される。profileの無い
/// nodeでは、perfのsymbolからGoのprocessのCPUを種類に分ける。
#[test]
fn go_profiles_and_runtime_values_reach_brief() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go-profile-stop.txt");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", """
printf '%s\n' '{{"type":"isuscope.event","name":"initialize-started"}}'
sleep 1
printf '%s\n' '{{"type":"isuscope.event","name":"initialize-finished"}}'
sleep 3
printf '%s\n' '{{"type":"isuscope.result","score":1,"pass":true}}'
"""]

[[collectors]]
name = "go-profile-stop"
phase = "after"
transport = "local"
command = ["cat", "{fixture}"]
parser = "go-profile"

# app-metrics-deltaがnodeから運ぶ、Goの書き手のruntimeの行と同じ形。
[[collectors]]
name = "app-metrics-delta"
phase = "after"
transport = "local"
command = ["sh", "-c", """
at() {{ awk -v origin='{{load_started_at}}' -v offset="$1" 'BEGIN {{ printf "%.3f", origin + offset }}'; }}
emit() {{ printf '{{"type":"metric","name":"%s","value":%s,"unit":"x","labels":{{}},"timestamp":%s}}\n' "$1" "$2" "$3"; }}
emit app.go.gc_cpu_ms 900 "$(at -0.5)"
for second in 1 2; do
  emit app.go.gc_cpu_ms 250 "$(at $second)"
  emit app.go.user_cpu_ms 750 "$(at $second)"
  emit app.go.mutex_wait_ms 40 "$(at $second)"
  emit app.go.sched_waits 1000 "$(at $second)"
  emit app.go.sched_wait_ms 20 "$(at $second)"
  emit app.go.sched_wait_p99_ms 0.$second "$(at $second)"
  emit app.go.goroutines $((100 * second)) "$(at $second)"
done
"""]

# perfのsymbol別の割合（時刻なし）。profileの無いnode app9のGoのprocess。
[[collectors]]
name = "perf-series"
phase = "after"
transport = "local"
command = ["sh", "-c", """
emit() {{ printf '{{"type":"metric","name":"cpu.sample_percent","value":%s,"unit":"percent","labels":{{"node":"app9","process":"%s","binary":"%s","symbol":"%s"}}}}\n' "$1" "$2" "$3" "$4"; }}
emit 30 isuride isuride runtime.scanobject
emit 10 isuride isuride runtime.mallocgc
emit 40 isuride isuride main.matchAll
emit 20 isuride '[kernel.kallsyms]' do_syscall_64
emit 50 nginx nginx ngx_http_process_request
"""]
"#,
            fixture = fixture.display()
        ),
    )
    .unwrap();
    let isuscope = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args(args)
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        parsed(&output.stdout).unwrap_or_default()
    };
    isuscope(&["run", "--hypothesis", "go profile"]);
    let brief = isuscope(&["brief", "latest"]);
    let rows = |section: &str| {
        brief[section]["rows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };

    let cpu = rows("go_cpu");
    assert_eq!(
        cpu[0]["function"], "crypto/internal/fips140/sha256.blockGeneric",
        "{}",
        brief["go_cpu"]
    );
    assert_eq!(
        cpu[0]["flat_percent"].as_f64(),
        Some(52.5),
        "{}",
        brief["go_cpu"]
    );
    let app = rows("go_cpu_app");
    assert_eq!(
        app[0]["function"], "main.main.func1",
        "{}",
        brief["go_cpu_app"]
    );
    let routes = rows("go_routes");
    let route = |name: &str| {
        routes
            .iter()
            .find(|row| row["route"] == name)
            .unwrap_or_else(|| panic!("{name} not in {}", brief["go_routes"]))
            .clone()
    };
    assert_eq!(
        route("/api/rides/:id")["percent"].as_f64(),
        Some(30.7),
        "{}",
        brief["go_routes"]
    );
    assert_eq!(
        route("/api/rides/:key")["percent"].as_f64(),
        Some(30.3),
        "{}",
        brief["go_routes"]
    );
    assert_eq!(
        route("/api/rides/:id")["method"],
        "GET",
        "{}",
        brief["go_routes"]
    );
    // 要求の外のgoroutine（GC、窓口自身など）はlabelが無い。
    assert_eq!(route("(no route)")["method"], "", "{}", brief["go_routes"]);
    let waits = rows("go_waits");
    let mutex = waits
        .iter()
        .find(|row| row["kind"] == "mutex" || brief["go_waits"]["common"]["kind"] == "mutex")
        .unwrap();
    assert_eq!(mutex["site"], "main.main.func1", "{}", brief["go_waits"]);
    assert!(!rows("go_allocs").is_empty(), "{}", brief["go_allocs"]);

    // pprofのあるnode（local）はprofileから、無いnode（app9）はperfから。
    let classes = rows("go_classes");
    let class = |source: &str, name: &str| {
        classes
            .iter()
            .find(|row| row["source"] == source && row["class"] == name)
            .and_then(|row| row["percent"].as_f64())
    };
    assert!(
        class("pprof", "crypto").is_some(),
        "{}",
        brief["go_classes"]
    );
    assert_eq!(class("perf", "gc"), Some(30.0), "{}", brief["go_classes"]);
    assert_eq!(
        class("perf", "alloc"),
        Some(10.0),
        "{}",
        brief["go_classes"]
    );
    assert_eq!(class("perf", "app"), Some(40.0), "{}", brief["go_classes"]);
    assert_eq!(
        class("perf", "kernel"),
        Some(20.0),
        "{}",
        brief["go_classes"]
    );
    assert!(
        classes.iter().all(|row| row["process"] != "nginx"),
        "{}",
        brief["go_classes"]
    );

    // runtimeの値は負荷区間だけ（initializeの間のgc_cpu_ms 900は入らない）。
    let runtime = &rows("app_runtime")[0];
    assert_eq!(
        brief["app_runtime"]["window"], "load",
        "{}",
        brief["app_runtime"]
    );
    assert_eq!(
        runtime["gc_cpu_percent"].as_f64(),
        Some(25.0),
        "{}",
        brief["app_runtime"]
    );
    assert_eq!(
        runtime["sched_wait_avg_us"].as_f64(),
        Some(20.0),
        "{}",
        brief["app_runtime"]
    );
    assert_eq!(
        runtime["sched_wait_p99_max_ms"].as_f64(),
        Some(0.2),
        "{}",
        brief["app_runtime"]
    );
    assert_eq!(
        runtime["goroutines_max"].as_f64(),
        Some(200.0),
        "{}",
        brief["app_runtime"]
    );
}

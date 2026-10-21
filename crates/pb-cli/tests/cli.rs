//! End-to-end tests of the `protobench` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_protobench"))
}

fn demo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor")
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pb-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn run_smoke_exits_0_and_writes_reports() {
    let reports = tmp("reports");
    let out = bin()
        .args(["run", "--project"])
        .arg(demo())
        .args(["--sequence", "smoke", "--profile", "sim", "--report-dir"])
        .arg(&reports)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let files: Vec<_> = std::fs::read_dir(&reports)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    let json = files
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .unwrap();
    let xml = files
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "xml"))
        .unwrap();
    let j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap();
    assert_eq!(j["passed"], true);
    assert!(std::fs::read_to_string(xml)
        .unwrap()
        .contains("<testsuite name=\"Bring-up smoke test\" tests=\"10\" failures=\"0\""));
    let _ = std::fs::remove_dir_all(reports);
}

#[test]
fn run_var_sets_operator_inputs() {
    let reports = tmp("vars");
    let run = |var: &str| {
        bin()
            .args(["run", "--project"])
            .arg(demo())
            .args(["--sequence", "smoke", "--profile", "sim", "--var", var])
            .arg("--report-dir")
            .arg(&reports)
            .output()
            .unwrap()
    };
    let out = run("load_ohms=4700");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("log: input load_ohms = 4700.0"), "{err}");
    assert!(err.contains("log: Done with a 4700.0 Ω load"), "{err}");
    // Below the input's min: the step fails, exit 1.
    let out = run("load_ohms=0");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("input `load_ohms`: 0 is below min 1"), "{err}");
    let out = run("load_ohms");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--var expects NAME=VALUE"));
    let _ = std::fs::remove_dir_all(reports);
}

/// Copies a folder tree, skipping recordings and reports.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let name = e.file_name();
        if name == "recordings" || name == "reports" {
            continue;
        }
        if p.is_dir() {
            copy_tree(&p, &to.join(&name));
        } else {
            std::fs::copy(&p, to.join(&name)).unwrap();
        }
    }
}

/// Copies the demo project and the reference meter it lists as device
/// `ref` (as siblings) into `dir`; returns the demo project's folder.
fn copy_demo(dir: &Path) -> PathBuf {
    let examples = demo().parent().unwrap().to_path_buf();
    copy_tree(&demo(), &dir.join("demo-sensor"));
    copy_tree(
        &examples.join("demo-reference"),
        &dir.join("demo-reference"),
    );
    dir.join("demo-sensor")
}

#[test]
fn failing_sequence_exits_1_and_bad_project_exits_2() {
    let root = tmp("proj");
    let dir = copy_demo(&root);
    std::fs::write(
        dir.join("sequences/bad.toml"),
        "[[steps]]\naction = \"send\"\ncommand = \"Set Bias\"\nexpect = \"ack\"\nassert = [ { path = \"ack.ok\", eq = false } ]\n",
    )
    .unwrap();
    let out = bin()
        .args(["run", "--project"])
        .arg(&dir)
        .args(["--sequence", "bad", "--profile", "sim", "--report-dir"])
        .arg(dir.join("r"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Unknown profile / unreachable device: configuration or connection error.
    let out = bin()
        .args(["run", "--project"])
        .arg(&dir)
        .args(["--sequence", "smoke", "--profile", "nope"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = bin()
        .args(["run", "--project"])
        .arg(&dir)
        .args([
            "--sequence",
            "smoke",
            "--profile",
            "net",
            "--port",
            "127.0.0.1:1",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Broken proto: readable error with file and line, exit 2.
    std::fs::write(
        dir.join("proto/device.proto"),
        "syntax = \"proto3\";\nmessage {\n",
    )
    .unwrap();
    let out = bin()
        .args(["run", "--project"])
        .arg(&dir)
        .args(["--sequence", "smoke", "--profile", "sim"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("device.proto:2"), "{err}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn monitor_record_then_decode_json_and_csv() {
    let d = tmp("rec");
    let rec = d.join("s.pbsession");
    let out = bin()
        .args(["monitor", "--project"])
        .arg(demo())
        .args([
            "--profile",
            "sim",
            "--send",
            "Start Stream",
            "--duration",
            "0.4",
            "--json",
            "--record",
        ])
        .arg(&rec)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let live = String::from_utf8_lossy(&out.stdout);
    assert!(live.lines().filter(|l| l.contains("\"telemetry\"")).count() > 100);
    let out = bin()
        .arg("decode")
        .arg(&rec)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let decoded = String::from_utf8_lossy(&out.stdout);
    let tele = decoded
        .lines()
        .filter(|l| l.contains("\"name\":\"telemetry\""))
        .count();
    assert!(tele > 100, "{tele}");
    assert!(decoded.lines().any(|l| l.contains("\"kind\":\"tx_frame\"")
        && l.contains("stream_ctrl")
        || l.contains("streamCtrl")));
    let csv = d.join("s.csv");
    let out = bin()
        .arg("decode")
        .arg(&rec)
        .arg("--csv")
        .arg(&csv)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = std::fs::read_to_string(&csv).unwrap();
    let mut lines = text.lines();
    let header = lines.next().unwrap();
    assert!(
        header.starts_with("time_s,") && header.contains("telemetry.vout_raw"),
        "{header}"
    );
    assert!(lines.count() > 100);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn schema_and_list_ports() {
    let out = bin()
        .args(["schema", "--project"])
        .arg(demo())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("message demo.HostToDevice"));
    assert!(s.contains("enum demo.SensorState"));
    assert!(!s.contains("google.protobuf"));
    let out = bin().arg("list-ports").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let out = bin().args(["list-ports", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for p in v.as_array().unwrap() {
        assert!(p.get("serial_number").is_some(), "{p}");
    }
}

#[test]
fn check_demo_passes_and_broken_sequence_fails_with_location() {
    let out = bin()
        .args(["check", "--project"])
        .arg(demo())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("0 errors"));

    let root = tmp("check");
    let dir = copy_demo(&root);
    std::fs::write(
        dir.join("sequences/broken.toml"),
        "name = \"broken\"\n\n[[steps]]\naction = \"send\"\ncommand = \"Pnig\"\n",
    )
    .unwrap();
    let out = bin()
        .args(["check", "--project"])
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().any(|l| l.contains("sequences/broken.toml:3: error: step 1 (send): no command `Pnig`")),
        "{text}"
    );
    let out = bin()
        .args(["check", "--json", "--project"])
        .arg(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["errors"], 1);
    assert_eq!(j["findings"][0]["line"], 3);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn decode_channels_range_and_per_channel() {
    let d = tmp("decode-opts");
    let rec = d.join("s.pbsession");
    let out = bin()
        .args(["monitor", "--project"])
        .arg(demo())
        .args([
            "--profile",
            "sim",
            "--send",
            "Start Stream",
            "--duration",
            "0.6",
            "--record",
        ])
        .arg(&rec)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Full export: the time span of the recording.
    let all = d.join("all.csv");
    let out = bin()
        .arg("decode")
        .arg(&rec)
        .arg("--csv")
        .arg(&all)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = std::fs::read_to_string(&all).unwrap();
    let times: Vec<f64> = text
        .lines()
        .skip(1)
        .filter_map(|l| l.split(',').next()?.parse().ok())
        .collect();
    let (t0, t1) = (times[0], *times.last().unwrap());
    let mid = (t0 + t1) / 2.0;

    let part = d.join("part.csv");
    let out = bin()
        .arg("decode")
        .arg(&rec)
        .args([
            "--channels",
            "telemetry.vout_raw,telemetry.state",
            "--range",
        ])
        .arg(format!("{mid}:"))
        .arg("--csv")
        .arg(&part)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&part).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        "time_s,telemetry.vout_raw,telemetry.state"
    );
    let rows: Vec<f64> = lines
        .filter_map(|l| l.split(',').next()?.parse().ok())
        .collect();
    assert!(
        !rows.is_empty() && rows.iter().all(|&t| t >= mid),
        "{rows:?}"
    );
    assert!(rows.len() < times.len());

    let per = d.join("per.csv");
    let out = bin()
        .arg("decode")
        .arg(&rec)
        .args([
            "--per-channel",
            "--channels",
            "telemetry.vout_raw,telemetry.current_na",
            "--csv",
        ])
        .arg(&per)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written = String::from_utf8_lossy(&out.stderr);
    assert_eq!(written.matches("wrote ").count(), 2, "{written}");

    let out = bin()
        .arg("decode")
        .arg(&rec)
        .args(["--channels", "ghost", "--csv"])
        .arg(d.join("x.csv"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no channel `ghost`"));
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn run_scripted_exits_0() {
    let reports = tmp("scripted");
    let out = bin()
        .args(["run", "--project"])
        .arg(demo())
        .args(["--sequence", "scripted", "--profile", "sim", "--report-dir"])
        .arg(&reports)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(reports);
}

#[test]
fn monitor_serves_foxglove() {
    use std::io::BufRead;
    use tungstenite::client::IntoClientRequest;
    let mut child = bin()
        .args(["monitor", "--project"])
        .arg(demo())
        .args(["--profile", "sim", "--duration", "20", "--foxglove", "0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    let url = loop {
        line.clear();
        assert!(err.read_line(&mut line).unwrap() > 0, "monitor exited");
        if let Some(rest) = line.split("foxglove server on ").nth(1) {
            break rest.split_whitespace().next().unwrap().to_string();
        }
    };
    assert!(url.starts_with("ws://127.0.0.1:"), "{url}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "foxglove.websocket.v1".parse().unwrap(),
    );
    let (mut ws, _) = tungstenite::connect(req).unwrap();
    let info = loop {
        if let tungstenite::Message::Text(t) = ws.read().unwrap() {
            break serde_json::from_str::<serde_json::Value>(t.as_str()).unwrap();
        }
    };
    assert_eq!(info["op"], "serverInfo");
    assert_eq!(info["name"], "ProtoBench: Demo Sensor");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn parquet_export_matches_csv() {
    use arrow_array::{Array, Float64Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let d = tmp("parquet");
    let rec = d.join("s.pbsession");
    let out = bin()
        .args(["monitor", "--project"])
        .arg(demo())
        .args([
            "--profile",
            "sim",
            "--send",
            "Start Stream",
            "--duration",
            "0.5",
            "--record",
        ])
        .arg(&rec)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (csv, pq) = (d.join("c.csv"), d.join("c.parquet"));
    let out = bin()
        .arg("decode")
        .arg(&rec)
        .arg("--csv")
        .arg(&csv)
        .arg("--parquet")
        .arg(&pq)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = std::fs::read_to_string(&csv).unwrap();
    let mut lines = text.lines();
    let header: Vec<String> = lines
        .next()
        .unwrap()
        .split(',')
        .map(str::to_string)
        .collect();
    let csv_rows: Vec<Vec<Option<f64>>> = lines
        .map(|l| l.split(',').map(|c| c.parse().ok()).collect())
        .collect();

    let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&pq).unwrap())
        .unwrap()
        .build()
        .unwrap();
    let mut names = Vec::new();
    let mut pq_rows: Vec<Vec<Option<f64>>> = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        names = batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        let cols: Vec<&Float64Array> = batch
            .columns()
            .iter()
            .map(|c| c.as_any().downcast_ref::<Float64Array>().unwrap())
            .collect();
        for r in 0..batch.num_rows() {
            pq_rows.push(
                cols.iter()
                    .map(|c| (!c.is_null(r)).then(|| c.value(r)))
                    .collect(),
            );
        }
    }
    assert_eq!(names, header);
    assert!(csv_rows.len() > 100);
    assert_eq!(pq_rows, csv_rows);
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn compare_runs_on_two_devices_and_fails_fast_without_one() {
    let reports = tmp("compare");
    let run = |extra: &[&str]| {
        bin()
            .args(["run", "--project"])
            .arg(demo())
            .args(["--sequence", "compare"])
            .args(extra)
            .arg("--report-dir")
            .arg(&reports)
            .output()
            .unwrap()
    };
    let out = run(&["--profile", "sim", "--profile", "ref=sim"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("PASS 2 send ref:Ping"), "{err}");
    assert!(err.contains("err_mv mean in [-5, 5]"), "{err}");
    // Without the meter the run stops at the start: a connection error.
    let out = run(&["--profile", "sim", "--only", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("device `ref` is not connected"), "{err}");
    // Unknown devices in options are configuration errors.
    let out = run(&["--profile", "meter2=sim"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no device `meter2`"));
    let out = run(&["--only", "meter2"]);
    assert_eq!(out.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(reports);
}

#[test]
fn send_reaches_one_device_only() {
    let out = bin()
        .args(["send", "--project"])
        .arg(demo())
        .args([
            "--device",
            "ref",
            "--profile",
            "sim",
            "--command",
            "Ping",
            "--wait",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("SIM-REF-1"));
    let out = bin()
        .args(["send", "--project"])
        .arg(demo())
        .args([
            "--device",
            "ref",
            "--profile",
            "sim",
            "--command",
            "Set Bias",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no command `Set Bias` on device `ref`"));
}

#[test]
fn two_device_recording_decodes_with_device_tags() {
    let d = tmp("bench-rec");
    let rec = d.join("bench.pbsession");
    let out = bin()
        .args(["monitor", "--project"])
        .arg(demo())
        .args([
            "--profile",
            "sim",
            "--send",
            "Start Stream",
            "--send",
            "ref:Ping",
            "--duration",
            "0.4",
            "--json",
            "--record",
        ])
        .arg(&rec)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let live = String::from_utf8_lossy(&out.stdout);
    assert!(
        live.lines().any(|l| l.contains("\"device\":\"ref\"")),
        "live JSON tags ref"
    );
    let out = bin().arg("decode").arg(&rec).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("ref:reading"),
        "{}",
        &text[..text.len().min(2000)]
    );
    assert!(text.contains("ref:pong"));
    let csv = d.join("bench.csv");
    let out = bin()
        .arg("decode")
        .arg(&rec)
        .args([
            "--channels",
            "telemetry.vout_raw,ref.reading.vout_mv,err_mv",
            "--csv",
        ])
        .arg(&csv)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let head = std::fs::read_to_string(&csv).unwrap();
    assert_eq!(
        head.lines().next().unwrap(),
        "time_s,telemetry.vout_raw,ref.reading.vout_mv,err_mv"
    );
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn round3_recording_decodes() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pb-core/tests/fixtures/round3-demo.pbsession");
    let out = bin().arg("decode").arg(&fixture).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("# Demo Sensor — recorded with ProtoBench 0.1.0 — 306 records"));
    assert!(!text.contains("<"), "no undecoded records");
    let d = tmp("r3");
    let out = bin()
        .arg("decode")
        .arg(&fixture)
        .arg("--parquet")
        .arg(d.join("r3.parquet"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn sim_and_run_over_mqtt() {
    let broker = pb_sim::mqtt::MiniBroker::start().unwrap();
    let addr = broker.addr().to_string();
    let root = tmp("mqtt");
    let dir = copy_demo(&root);
    // One message per PUBLISH, and an mqtt profile.
    let cfg = dir.join("project.toml");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("type = \"cobs\"", "type = \"none\"")
        .replace("text_passthrough = true", "text_passthrough = false")
        .replace(
            "# ---------- other devices on the bench ----------",
            "[[connection.profiles]]\nname = \"mqtt\"\ntransport = \"mqtt\"\nbroker = \"127.0.0.1:1\"\nsubscribe = [\"bench/+/up\"]\npublish = \"bench/sim/down\"\nqos = 1\n\n# ---------- other devices on the bench ----------",
        );
    std::fs::write(&cfg, text).unwrap();
    let mut sim = bin()
        .args(["sim", "--project"])
        .arg(&dir)
        .args(["--mqtt", &addr])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The simulator is up once the broker has its connection.
    let t0 = std::time::Instant::now();
    while broker.connections() == 0 {
        assert!(t0.elapsed().as_secs() < 10, "simulator did not connect");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = bin()
        .args(["run", "--project"])
        .arg(&dir)
        .args([
            "--sequence",
            "smoke",
            "--profile",
            "mqtt",
            "--port",
            &addr,
            "--only",
            "main",
            "--report-dir",
        ])
        .arg(root.join("r"))
        .output()
        .unwrap();
    let _ = sim.kill();
    let _ = sim.wait();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("mqtt://"), "{err}");
    let _ = std::fs::remove_dir_all(root);
}

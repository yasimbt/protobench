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

/// Copies the demo project (one folder level deep) into `dir`.
fn copy_demo(dir: &Path) {
    for e in std::fs::read_dir(demo()).unwrap().flatten() {
        let p = e.path();
        let name = e.file_name();
        if name == "recordings" || name == "reports" {
            continue;
        }
        if p.is_dir() {
            let to = dir.join(&name);
            std::fs::create_dir_all(&to).unwrap();
            for f in std::fs::read_dir(&p).unwrap().flatten() {
                std::fs::copy(f.path(), to.join(f.file_name())).unwrap();
            }
        } else {
            std::fs::copy(&p, dir.join(&name)).unwrap();
        }
    }
}

#[test]
fn failing_sequence_exits_1_and_bad_project_exits_2() {
    let dir = tmp("proj");
    copy_demo(&dir);
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
    let _ = std::fs::remove_dir_all(dir);
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

    let dir = tmp("check");
    copy_demo(&dir);
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
    let _ = std::fs::remove_dir_all(dir);
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

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
fn failing_sequence_exits_1_and_bad_project_exits_2() {
    let dir = tmp("proj");
    for e in std::fs::read_dir(demo()).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            let to = dir.join(e.file_name());
            std::fs::create_dir_all(&to).unwrap();
            for f in std::fs::read_dir(&p).unwrap().flatten() {
                std::fs::copy(f.path(), to.join(f.file_name())).unwrap();
            }
        } else {
            std::fs::copy(&p, dir.join(e.file_name())).unwrap();
        }
    }
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

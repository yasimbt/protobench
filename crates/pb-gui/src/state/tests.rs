use std::time::Duration;

use super::*;
use pb_core::telemetry::TelemetryStore;

fn demo_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor")
}

fn connected() -> AppState {
    let mut s = AppState::default();
    s.open_project(&demo_dir());
    assert!(s.diagnostics.is_empty(), "{:?}", s.diagnostics);
    assert_eq!(s.profile.as_deref(), Some("usb"));
    s.connect("sim").unwrap();
    wait(&mut s, |s| s.connected());
    s
}

fn wait(s: &mut AppState, cond: impl Fn(&AppState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        s.poll();
        if cond(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached");
}

fn store(s: &AppState) -> Arc<TelemetryStore> {
    s.session.as_ref().unwrap().telemetry().clone()
}

#[test]
fn start_stream_fills_channels_and_text_console() {
    let mut s = connected();
    assert!(matches!(
        s.click_command("Start Stream"),
        ClickOutcome::Sent(_)
    ));
    wait(&mut s, |s| {
        matches!(s.badges.get("Start Stream"), Some(Badge::Ok { .. }))
    });
    wait(&mut s, |s| {
        store(s)
            .get("telemetry.vout_raw")
            .is_some_and(|c| c.total() > 100)
            && store(s).get("telemetry.state").is_some()
            && !s.text_log.is_empty()
    });
    assert!(s.msg_log.iter().any(|e| e.msg.kind() == "telemetry"));
    assert!(!s.raw_log.is_empty());
    assert_eq!(s.history.len(), 1);
}

#[test]
fn set_bias_prompts_then_shows_ok_badge() {
    let mut s = connected();
    assert_eq!(s.click_command("Set Bias"), ClickOutcome::Prompt);
    match &mut s.dialog {
        Some(Dialog::Prompt { fields, .. }) => {
            assert_eq!(fields, &vec![("millivolts".to_string(), "200".to_string())]);
            fields[0].1 = "350".into();
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(s.submit_prompt(), ClickOutcome::Sent(_)));
    assert!(s.dialog.is_none());
    wait(&mut s, |s| {
        matches!(s.badges.get("Set Bias"), Some(Badge::Ok { .. }))
    });
    assert_eq!(s.history.iter().last().unwrap().payload["millivolts"], 350);
}

#[test]
fn erase_calibration_asks_for_confirmation() {
    let mut s = connected();
    assert_eq!(s.click_command("Erase Calibration"), ClickOutcome::Confirm);
    assert!(matches!(s.dialog, Some(Dialog::Confirm { .. })));
    s.cancel_dialog();
    assert!(s.dialog.is_none());
    assert!(s.history.is_empty(), "cancel must not send");
    assert_eq!(s.click_command("Erase Calibration"), ClickOutcome::Confirm);
    assert!(matches!(s.confirm(), ClickOutcome::Sent(_)));
    wait(&mut s, |s| {
        matches!(s.badges.get("Erase Calibration"), Some(Badge::Sent))
    });
}

#[test]
fn periodic_heartbeat_and_resend() {
    let mut s = connected();
    s.set_periodic("Heartbeat", true).unwrap();
    assert!(s.is_periodic("Heartbeat"));
    wait(&mut s, |s| {
        matches!(s.badges.get("Heartbeat"), Some(Badge::Sent))
    });
    s.set_periodic("Heartbeat", false).unwrap();
    s.click_command("Ping");
    wait(&mut s, |s| {
        matches!(s.badges.get("Ping"), Some(Badge::Ok { .. }))
    });
    let n = s.history.len();
    s.resend(n - 1).unwrap();
    wait(&mut s, |s| s.history.len() == n + 1);
}

#[test]
fn recording_markers_and_csv_export() {
    let mut s = connected();
    let dir = std::env::temp_dir().join(format!("pb-gui-rec-{}", std::process::id()));
    s.settings.recordings_dir = Some(dir.clone());
    s.click_command("Start Stream");
    let path = s.start_recording().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    s.add_marker("load on");
    wait(&mut s, |s| !s.markers.is_empty());
    s.stop_recording();
    assert!(s.recording.is_none());
    let off = pb_core::recorder::OfflineSession::open(&path).unwrap();
    assert!(off.records.len() > 50);
    let csv = dir.join("out.csv");
    s.export_csv(
        csv.clone(),
        vec!["telemetry.vout_raw".into()],
        None,
        CsvMode::Joined,
    );
    wait(&mut s, |_| {
        csv.exists()
            && std::fs::read_to_string(&csv)
                .map(|t| t.lines().count() > 10)
                .unwrap_or(false)
    });
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(text.starts_with("time_s,telemetry.vout_raw\n"));
    let pq = dir.join("out.parquet");
    s.export_parquet(pq.clone());
    wait(&mut s, |s| {
        s.notifications
            .iter()
            .any(|(_, n)| n.contains("rows to") && n.contains("out.parquet"))
    });
    // A Parquet file starts and ends with the `PAR1` magic.
    let bytes = std::fs::read(&pq).unwrap();
    assert!(bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn broken_project_opens_with_errors_and_refuses_to_connect() {
    let dir = std::env::temp_dir().join(format!("pb-gui-broken-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    pb_core::project::create_project(&dir, "Broken").unwrap();
    std::fs::write(
        dir.join("proto/device.proto"),
        "syntax = \"proto3\";\nmessage {\n",
    )
    .unwrap();
    let mut s = AppState::default();
    s.open_project(&dir);
    assert!(s.project.is_some());
    assert!(s
        .diagnostics
        .iter()
        .any(|d| d.is_error() && d.line == Some(2)));
    assert!(s.connect("sim").is_err());
    std::fs::write(dir.join("project.toml"), "name = [\n").unwrap();
    s.open_project(&dir);
    assert!(s
        .diagnostics
        .iter()
        .any(|d| d.is_error() && d.line.is_some()));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn save_command_preserves_formatting() {
    let text =
        "# my project\nname = \"x\" # trailing\n\n[[commands]]\nname = \"A\"\nmessage = \"ping\"\n";
    let out = append_command(
        text,
        "B",
        "Saved",
        "set_bias",
        &serde_json::json!({"millivolts": 5}),
        Some("ack"),
    )
    .unwrap();
    assert!(out.starts_with("# my project\nname = \"x\" # trailing\n"));
    assert!(out.contains("[[commands]]\nname = \"B\"\ngroup = \"Saved\"\nmessage = \"set_bias\"\npayload = { millivolts = 5 }\nexpect = \"ack\"\n"), "{out}");
    let cfg = pb_core::config::load_config_str(&out, Path::new("p.toml"))
        .config
        .unwrap();
    assert_eq!(cfg.commands.len(), 2);
}

#[test]
fn level_detection() {
    assert_eq!(detect_level("E: boom"), Level::Error);
    assert_eq!(detect_level("[ERROR] boom"), Level::Error);
    assert_eq!(detect_level("W: hmm"), Level::Warn);
    assert_eq!(detect_level("[INFO] ok"), Level::Info);
    assert_eq!(detect_level("D: x"), Level::Debug);
    assert_eq!(detect_level("hello"), Level::Plain);
}

#[test]
fn schema_diff_reports_renumbering() {
    let dir = std::env::temp_dir().join(format!("pb-gui-diff-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.proto"),
        "syntax = \"proto3\";\npackage p;\nmessage M { int32 a = 1; int32 b = 2; }\n",
    )
    .unwrap();
    let a = pb_core::schema::Schema::compile(&dir, &["a.proto".into()], &[]).unwrap();
    std::fs::write(
        dir.join("a.proto"),
        "syntax = \"proto3\";\npackage p;\nmessage M { int32 a = 3; }\n",
    )
    .unwrap();
    let b = pb_core::schema::Schema::compile(&dir, &["a.proto".into()], &[]).unwrap();
    let d = schema_diff(&a, &b);
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d[0].contains("1 → 3"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn foxglove_toggle_follows_the_connection() {
    let mut s = connected();
    s.settings.foxglove_port = 0;
    s.set_foxglove(true).unwrap();
    let (addr, clients) = s.foxglove_status().expect("server running");
    assert_eq!(clients, 0);
    assert!(std::net::TcpStream::connect(addr).is_ok());
    // Reconnecting restarts it; disconnecting stops it but keeps it wanted.
    s.connect("sim").unwrap();
    wait(&mut s, |s| s.connected());
    assert!(s.foxglove_status().is_some());
    s.disconnect();
    assert!(s.foxglove_status().is_none());
    assert!(s.foxglove_enabled);
    s.connect("sim").unwrap();
    assert!(s.foxglove_status().is_some());
    s.set_foxglove(false).unwrap();
    assert!(s.foxglove_status().is_none());
}

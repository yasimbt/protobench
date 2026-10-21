//! `--smoke-test`: drives the real UI code paths against the simulator.
//!
//! 1. Headless: renders frames with `egui::Context::run_ui` (no display
//!    needed) while a script clicks Connect, Start Stream, Set Bias (prompt),
//!    Erase Calibration (confirm) and sends every composer message, through
//!    the same `AppState` actions the buttons call, then runs the `smoke`
//!    sequence and answers its operator dialog. It then checks that the
//!    plot, readout, indicator and text-console data are there.
//! 2. With `--window`: the same script in a real eframe window.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app::ProtoBenchApp;
use crate::layout::Panel;
use crate::state::{Badge, ClickOutcome, Dialog};
use crate::tools::{tx_targets, ComposerState};

/// The demo project, found relative to the working directory or the binary.
pub fn demo_dir() -> PathBuf {
    let rel = Path::new("examples/demo-sensor");
    if rel.join("project.toml").exists() {
        return rel.to_path_buf();
    }
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            let p = anc.join("examples/demo-sensor");
            if p.join("project.toml").exists() {
                return p;
            }
        }
    }
    rel.to_path_buf()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    WaitConnected,
    StartStream,
    WaitStream,
    SetBias,
    WaitBias,
    Erase,
    WaitErase,
    Composer,
    WaitComposer,
    Foxglove,
    Parquet,
    WaitParquet,
    Sequence,
    WaitOperator,
    WaitSequence,
    Render,
    Done,
}

/// The scripted clicks, advanced one step per frame.
struct Script {
    step: Step,
    started: Instant,
    frames_in_render: u32,
    frames_in_dialog: u32,
    composer_targets: Vec<String>,
    log: Vec<String>,
    result: Option<Result<String, String>>,
}

impl Script {
    fn new() -> Self {
        Script {
            step: Step::WaitConnected,
            started: Instant::now(),
            frames_in_render: 0,
            frames_in_dialog: 0,
            composer_targets: Vec::new(),
            log: Vec::new(),
            result: None,
        }
    }

    fn fail(&mut self, m: impl Into<String>) {
        self.result = Some(Err(format!("{} (at {:?})", m.into(), self.step)));
        self.step = Step::Done;
    }

    fn tick(&mut self, app: &mut ProtoBenchApp) {
        if self.step == Step::Done {
            return;
        }
        if self.started.elapsed() > Duration::from_secs(30) {
            self.fail("timed out");
            return;
        }
        let s = &mut app.state;
        match self.step {
            Step::WaitConnected => {
                if s.connected() {
                    self.log.push("connected to the simulator".into());
                    self.step = Step::StartStream;
                }
            }
            Step::StartStream => match s.click_command("Start Stream") {
                ClickOutcome::Sent(_) => self.step = Step::WaitStream,
                other => self.fail(format!("Start Stream: {other:?}")),
            },
            Step::WaitStream => {
                let ok = matches!(s.badges.get("Start Stream"), Some(Badge::Ok { .. }));
                let store = s.session.as_ref().map(|x| x.telemetry().clone());
                let data = store.is_some_and(|t| {
                    t.get("telemetry.vout_raw").is_some_and(|c| c.total() > 200)
                        && t.get("telemetry.state").is_some()
                });
                if ok && data && !s.text_log.is_empty() {
                    self.log
                        .push("Start Stream ✔, telemetry and text lines flowing".into());
                    self.step = Step::SetBias;
                }
            }
            Step::SetBias => {
                if s.click_command("Set Bias") != ClickOutcome::Prompt {
                    return self.fail("Set Bias did not open the prompt dialog");
                }
                if let Some(Dialog::Prompt { fields, .. }) = &mut s.dialog {
                    fields[0].1 = "250".into();
                }
                match s.submit_prompt() {
                    ClickOutcome::Sent(_) => self.step = Step::WaitBias,
                    other => self.fail(format!("prompt submit: {other:?}")),
                }
            }
            Step::WaitBias => match s.badges.get("Set Bias") {
                Some(Badge::Ok { .. }) => {
                    self.log
                        .push("Set Bias opened the prompt dialog and shows ✔".into());
                    self.step = Step::Erase;
                }
                Some(Badge::Timeout) | Some(Badge::Error(_)) => self.fail("Set Bias failed"),
                _ => {}
            },
            Step::Erase => {
                if s.click_command("Erase Calibration") != ClickOutcome::Confirm
                    || !matches!(s.dialog, Some(Dialog::Confirm { .. }))
                {
                    return self.fail("Erase Calibration did not ask for confirmation");
                }
                match s.confirm() {
                    ClickOutcome::Sent(_) => self.step = Step::WaitErase,
                    other => self.fail(format!("confirm: {other:?}")),
                }
            }
            Step::WaitErase => {
                if matches!(s.badges.get("Erase Calibration"), Some(Badge::Sent)) {
                    self.log
                        .push("Erase Calibration asked for confirmation, then sent".into());
                    self.step = Step::Composer;
                }
            }
            Step::Composer => {
                app.windows.composer = true;
                let Some(codec) = s.session.as_ref().map(|x| (**x.codec()).clone()) else {
                    return self.fail("no session");
                };
                for t in tx_targets(&codec).into_iter().filter(|t| !t.contains('.')) {
                    let mut cs = ComposerState::default();
                    if let Err(e) = cs.select(&codec, &t) {
                        return self.fail(e);
                    }
                    if t == "stream_ctrl" {
                        // Keep the stream running for the rest of the test.
                        if let Some(m) = cs.msg.as_mut() {
                            let _ =
                                m.try_set_field_by_name("enable", prost_reflect::Value::Bool(true));
                            let _ =
                                m.try_set_field_by_name("rate_hz", prost_reflect::Value::U32(1000));
                        }
                    }
                    if let Err(e) = cs.send(s) {
                        return self.fail(format!("composer {t}: {e}"));
                    }
                    self.composer_targets.push(t);
                }
                // Leave the last one in the window so it renders.
                if let Some(t) = self.composer_targets.first() {
                    let _ = app.composer.select(&codec, t);
                }
                app.windows.schema = true;
                self.step = Step::WaitComposer;
            }
            Step::WaitComposer => {
                let done = self.composer_targets.iter().all(|t| {
                    matches!(
                        s.badges.get(&format!("composer: {t}")),
                        Some(Badge::Sent) | Some(Badge::Ok { .. })
                    )
                });
                if done {
                    self.log.push(format!(
                        "composer sent {} HostToDevice messages: {}",
                        self.composer_targets.len(),
                        self.composer_targets.join(", ")
                    ));
                    self.step = Step::Foxglove;
                }
            }
            Step::Foxglove => {
                // Tools > Foxglove server, on a free port.
                s.settings.foxglove_port = 0;
                if let Err(e) = s.set_foxglove(true) {
                    return self.fail(format!("Foxglove server: {e}"));
                }
                let Some((addr, _)) = s.foxglove_status() else {
                    return self.fail("Foxglove server not running");
                };
                if std::net::TcpStream::connect(addr).is_err() {
                    return self.fail(format!("Foxglove server at {addr} refuses connections"));
                }
                self.log
                    .push(format!("Foxglove server listening on ws://{addr}"));
                self.step = Step::Parquet;
            }
            Step::Parquet => {
                // Record > Export Parquet.
                s.export_parquet(smoke_parquet_path());
                self.step = Step::WaitParquet;
            }
            Step::WaitParquet => {
                let note = s
                    .notifications
                    .iter()
                    .rev()
                    .map(|(_, n)| n.clone())
                    .find(|n| n.contains("smoke.parquet"));
                if let Some(n) = note {
                    let path = smoke_parquet_path();
                    let ok = std::fs::read(&path)
                        .map(|b| b.starts_with(b"PAR1") && b.ends_with(b"PAR1"))
                        .unwrap_or(false);
                    let _ = std::fs::remove_file(&path);
                    if !ok {
                        return self.fail(format!("Parquet export: {n}"));
                    }
                    self.log.push(format!("Parquet export: {n}"));
                    s.set_foxglove(false).ok();
                    self.step = Step::Sequence;
                }
            }
            Step::Sequence => {
                app.windows.runner = true;
                app.runner.select("smoke");
                match app.runner.start(&app.state, "smoke") {
                    Ok(()) => self.step = Step::WaitOperator,
                    Err(e) => self.fail(format!("sequence runner: {e}")),
                }
            }
            Step::WaitOperator => {
                let Some(Dialog::Operator { fields, .. }) = &mut s.dialog else {
                    if !app.runner.is_running() {
                        self.fail("smoke sequence finished without an operator dialog");
                    }
                    return;
                };
                if fields.len() != 1 || fields[0].text != "10000" {
                    return self.fail(format!("operator dialog fields: {fields:?}"));
                }
                // Let the dialog render a few frames before answering.
                self.frames_in_dialog += 1;
                if self.frames_in_dialog >= 5 {
                    fields[0].text = "4700".into();
                    s.answer_operator(true);
                    self.step = Step::WaitSequence;
                }
            }
            Step::WaitSequence => {
                let Some(r) = &app.runner.report else { return };
                if !r.passed {
                    return self.fail(format!("smoke sequence failed: {}", r.summary_text()));
                }
                if !r.log.iter().any(|l| l == "input load_ohms = 4700.0") {
                    return self.fail(format!("operator input not logged: {:?}", r.log));
                }
                self.log.push(format!(
                    "smoke sequence passed with the operator entering load_ohms = 4700 ({} steps)",
                    r.summary.total
                ));
                self.step = Step::Render;
            }
            Step::Render => {
                if self.frames_in_render == 0 {
                    app.windows.composer = false;
                    app.windows.schema = false;
                }
                self.frames_in_render += 1;
                if self.frames_in_render >= 60 {
                    match verify(app) {
                        Ok(m) => {
                            self.log.push(m);
                            self.result = Some(Ok(self.log.join("\n")));
                        }
                        Err(e) => self.result = Some(Err(e)),
                    }
                    self.step = Step::Done;
                }
            }
            Step::Done => {}
        }
    }
}

fn smoke_parquet_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "protobench-smoke-{}.smoke.parquet",
        std::process::id()
    ))
}

fn verify(app: &ProtoBenchApp) -> Result<String, String> {
    let s = &app.state;
    let store = s
        .session
        .as_ref()
        .ok_or("disconnected")?
        .telemetry()
        .clone();
    let tabs: Vec<Panel> = app
        .dock
        .iter_all_tabs()
        .map(|(_, t)| t.panel.clone())
        .collect();
    let plot = tabs
        .iter()
        .find_map(|p| match p {
            Panel::TimePlot(c) => Some(c.clone()),
            _ => None,
        })
        .ok_or("default layout has no time plot")?;
    for t in &plot.traces {
        let ch = store
            .get(&t.channel)
            .ok_or(format!("plot channel {} has no data", t.channel))?;
        if ch.total() < 100 {
            return Err(format!(
                "plot channel {} has only {} samples",
                t.channel,
                ch.total()
            ));
        }
    }
    if !tabs.iter().any(|p| matches!(p, Panel::Readouts(_))) {
        return Err("no readouts panel".into());
    }
    let ind = tabs
        .iter()
        .find_map(|p| match p {
            Panel::Indicators(c) => Some(c.clone()),
            _ => None,
        })
        .ok_or("no indicators panel")?;
    for c in &ind.channels {
        store
            .get(c)
            .ok_or(format!("indicator {c} has no data"))?
            .last()
            .ok_or("indicator empty")?;
    }
    if s.text_log.is_empty() {
        return Err("text console is empty".into());
    }
    let errors = s.session.as_ref().map(|x| x.stats().errors()).unwrap_or(0);
    Ok(format!(
        "plot traces {:?} live, {} channels, {} text lines, {} messages logged, link errors {errors}",
        plot.traces.iter().map(|t| t.channel.as_str()).collect::<Vec<_>>(),
        store.channels().len(),
        s.text_log.len(),
        s.msg_log.len()
    ))
}

fn new_app(dir: &Path) -> ProtoBenchApp {
    let mut app = ProtoBenchApp::new(None, None, None);
    app.persist_layout = false;
    app.open_project(dir.to_path_buf());
    if let Err(e) = app.state.connect("sim") {
        app.state.notify(format!("connect failed: {e}"));
    }
    app
}

/// Runs the smoke test. Returns a summary.
pub fn run(dir: &Path, window: bool) -> Result<String, String> {
    let summary = run_headless(dir)?;
    if !window {
        return Ok(format!(
            "{summary}\n(headless rendering; pass --window to also open a real window)"
        ));
    }
    match run_window(dir) {
        Ok(s) => Ok(format!("{summary}\nwindow: {s}")),
        Err(e) if e.starts_with("no display") => Ok(format!("{summary}\nwindow skipped: {e}")),
        Err(e) => Err(e),
    }
}

fn run_headless(dir: &Path) -> Result<String, String> {
    let mut app = new_app(dir);
    if app.state.project.as_ref().is_none_or(|p| p.has_errors()) {
        return Err(format!(
            "project {} has errors: {:?}",
            dir.display(),
            app.state.diagnostics
        ));
    }
    let ctx = egui::Context::default();
    let mut script = Script::new();
    let t0 = Instant::now();
    let mut frames = 0u32;
    while script.result.is_none() {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            time: Some(t0.elapsed().as_secs_f64()),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.frame(ui));
        // No renderer: drop the texture uploads.
        out.textures_delta.clear();
        script.tick(&mut app);
        frames += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    app.state.disconnect();
    script
        .result
        .unwrap_or_else(|| Err("no result".into()))
        .map(|s| {
            format!(
                "{s}\nheadless: {frames} frames rendered in {:.1} s",
                t0.elapsed().as_secs_f64()
            )
        })
}

fn run_window(dir: &Path) -> Result<String, String> {
    if std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
        && !cfg!(any(windows, target_os = "macos"))
    {
        return Err("no display (DISPLAY/WAYLAND_DISPLAY unset)".into());
    }
    let result: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
    let r2 = result.clone();
    let dir = dir.to_path_buf();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ProtoBench smoke test")
            .with_inner_size([1400.0, 900.0]),
        persistence_path: Some(std::env::temp_dir().join("protobench-smoke-storage.ron")),
        ..Default::default()
    };
    let started = eframe::run_native(
        "ProtoBench smoke test",
        options,
        Box::new(move |_cc| {
            let mut app = new_app(&dir);
            let mut script = Script::new();
            let res = r2.clone();
            let shot_path = std::env::var_os("PROTOBENCH_SCREENSHOT").map(PathBuf::from);
            let mut shot_requested = false;
            let mut shot_saved = false;
            app.hook = Some(Box::new(
                move |app: &mut ProtoBenchApp, ctx: &egui::Context| {
                    if let Some(path) = shot_path.as_ref().filter(|_| !shot_saved) {
                        // Hold the result until the screenshot is saved.
                        if script.step == Step::Render && script.frames_in_render >= 25 {
                            if !shot_requested {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                                    Default::default(),
                                ));
                                shot_requested = true;
                            }
                            let img = ctx.input(|i| {
                                i.raw.events.iter().find_map(|e| match e {
                                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                                    _ => None,
                                })
                            });
                            match img {
                                Some(img) => {
                                    if let Err(e) = write_png(path, &img) {
                                        eprintln!("screenshot: {e}");
                                    }
                                    shot_saved = true;
                                }
                                None => {
                                    ctx.request_repaint();
                                    return;
                                }
                            }
                        }
                    }
                    script.tick(app);
                    if let Some(r) = script.result.take() {
                        *res.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                        script.step = Step::Done;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ctx.request_repaint();
                },
            ));
            Ok(Box::new(app))
        }),
    );
    if let Err(e) = started {
        return Err(format!("no display: cannot open a window ({e})"));
    }
    let r = result.lock().unwrap_or_else(|e| e.into_inner()).take();
    r.unwrap_or_else(|| Err("window closed before the script finished".into()))
}

/// Writes an RGBA image as an uncompressed PNG (no extra dependencies).
fn write_png(path: &Path, img: &egui::ColorImage) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let [w, h] = img.size;
    let mut raw = Vec::with_capacity((w * 4 + 1) * h);
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&img.pixels[y * w + x].to_array());
        }
    }
    // zlib with stored (uncompressed) deflate blocks.
    let mut z = vec![0x78, 0x01];
    for (i, block) in raw.chunks(65_535).enumerate() {
        let last = (i + 1) * 65_535 >= raw.len();
        z.push(u8::from(last));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}

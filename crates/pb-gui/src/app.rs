//! The eframe application: menus, status bar, dock area, dialogs and tool
//! windows. Actions go through [`AppState`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Color32, RichText};
use egui_dock::{DockArea, DockState, Style};

use crate::layout::{
    local_layout_path, project_layout, save_layout, startup_layout, LayoutSource, Panel, Tab,
    TabIds,
};
use crate::panels::Viewer;
use crate::state::{AppState, Dialog, Settings};
use crate::tools::{ComposerState, ProjectEditorState, RunnerState, SchemaBrowserState};

/// Requests from panels and menus, handled after drawing.
#[derive(Debug, Clone)]
pub enum Action {
    /// Add a panel to the focused leaf.
    AddPanel(Panel),
    /// Show (or add) the link statistics panel.
    ShowLinkStats,
    /// Go back to the project's `layout.json` (or the built-in default).
    ResetLayout,
    /// Write the current layout to the project's `layout.json`.
    SaveProjectLayout,
}

/// Callback run at the start of every frame.
pub type FrameHook = Box<dyn FnMut(&mut ProtoBenchApp, &egui::Context)>;

/// Which tool windows are open.
#[derive(Debug, Default, Clone, Copy)]
pub struct Windows {
    /// Schema browser.
    pub schema: bool,
    /// Message composer.
    pub composer: bool,
    /// Sequence runner.
    pub runner: bool,
    /// Settings.
    pub settings: bool,
    /// Project editor.
    pub editor: bool,
    /// About.
    pub about: bool,
    /// Error panel.
    pub errors: bool,
}

/// The application.
pub struct ProtoBenchApp {
    /// State and actions.
    pub state: AppState,
    /// Dock layout.
    pub dock: DockState<Tab>,
    ids: TabIds,
    /// Open windows.
    pub windows: Windows,
    /// Composer.
    pub composer: ComposerState,
    schema_browser: SchemaBrowserState,
    /// Sequence runner.
    pub runner: RunnerState,
    editor: ProjectEditorState,
    actions: Vec<Action>,
    saved_layout: String,
    last_layout_check: Instant,
    marker_text: String,
    applied_dark: Option<bool>,
    /// Write `layout.json` when the layout changes (off in the smoke test).
    pub persist_layout: bool,
    /// Optional hook run every frame (used by `--smoke-test`).
    pub hook: Option<FrameHook>,
}

const STORAGE_SETTINGS: &str = "protobench.settings";
const STORAGE_RECENT: &str = "protobench.recent";

impl ProtoBenchApp {
    /// Creates the app, optionally opening a project and connecting.
    pub fn new(
        cc: Option<&eframe::CreationContext<'_>>,
        project: Option<PathBuf>,
        profile: Option<String>,
    ) -> Self {
        let (settings, recent) = match cc.and_then(|c| c.storage) {
            Some(s) => (
                eframe::get_value::<Settings>(s, STORAGE_SETTINGS).unwrap_or_default(),
                eframe::get_value::<Vec<PathBuf>>(s, STORAGE_RECENT).unwrap_or_default(),
            ),
            None => (Settings::default(), Vec::new()),
        };
        let mut state = AppState::new(settings);
        state.recent = recent;
        let dock = crate::layout::default_layout(None);
        let ids = TabIds::after(&dock);
        let mut app = ProtoBenchApp {
            state,
            dock,
            ids,
            windows: Windows::default(),
            composer: ComposerState::default(),
            schema_browser: SchemaBrowserState::default(),
            runner: RunnerState::default(),
            editor: ProjectEditorState::default(),
            actions: Vec::new(),
            saved_layout: String::new(),
            last_layout_check: Instant::now(),
            marker_text: String::new(),
            applied_dark: None,
            persist_layout: true,
            hook: None,
        };
        if let Some(p) = project {
            app.open_project(p);
            if let Some(prof) = profile {
                if let Err(e) = app.state.connect(&prof) {
                    app.state.notify(format!("connect failed: {e}"));
                }
            }
        }
        app
    }

    /// Opens a project and its layout.
    pub fn open_project(&mut self, dir: PathBuf) {
        self.save_layout_now();
        self.state.open_project(&dir);
        self.load_project_layout();
        self.composer = ComposerState::default();
        self.runner = RunnerState::default();
        self.editor = ProjectEditorState::default();
        self.windows.errors = self.state.diagnostics.iter().any(|d| d.is_error());
    }

    fn load_project_layout(&mut self) {
        let Some(p) = self.state.project.clone() else {
            return;
        };
        let mut warnings = Vec::new();
        let (dock, source) = startup_layout(&p, &mut warnings);
        for w in warnings {
            self.state.notify(w);
        }
        if source == LayoutSource::Default {
            self.state.notify("using the default dashboard");
        }
        self.dock = dock;
        self.ids = TabIds::after(&self.dock);
        self.saved_layout = layout_fingerprint(&self.dock);
    }

    /// Saves the working layout (`layout.local.json`) if it changed.
    /// The project's `layout.json` is only written on request.
    pub fn save_layout_now(&mut self) {
        if !self.persist_layout {
            return;
        }
        let Some(p) = &self.state.project else { return };
        let fp = layout_fingerprint(&self.dock);
        if fp == self.saved_layout {
            return;
        }
        match save_layout(&local_layout_path(p), &self.dock) {
            Ok(()) => self.saved_layout = fp,
            Err(e) => self.state.notify(format!("cannot save layout: {e}")),
        }
    }

    fn handle_actions(&mut self) {
        for a in std::mem::take(&mut self.actions) {
            match a {
                Action::AddPanel(p) => {
                    let tab = self.ids.tab(p);
                    self.dock.push_to_focused_leaf(tab);
                }
                Action::ShowLinkStats => {
                    let found = self
                        .dock
                        .find_tab_from(|t| matches!(t.panel, Panel::LinkStats));
                    match found {
                        Some(path) => {
                            let _ = self.dock.set_active_tab(path);
                        }
                        None => {
                            let tab = self.ids.tab(Panel::LinkStats);
                            self.dock.push_to_focused_leaf(tab);
                        }
                    }
                }
                Action::ResetLayout => {
                    let Some(p) = self.state.project.clone() else {
                        continue;
                    };
                    let mut warnings = Vec::new();
                    self.dock = project_layout(&p, &mut warnings).0;
                    self.ids = TabIds::after(&self.dock);
                    for w in warnings {
                        self.state.notify(w);
                    }
                }
                Action::SaveProjectLayout => {
                    let Some(p) = self.state.project.clone() else {
                        continue;
                    };
                    match save_layout(&p.layout_path(), &self.dock) {
                        Ok(()) => self
                            .state
                            .notify(format!("saved {}", p.layout_path().display())),
                        Err(e) => self.state.notify(format!("cannot save layout: {e}")),
                    }
                }
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let Some(p) = &self.state.project else { return };
        let cmds: Vec<(String, egui::KeyboardShortcut)> = p
            .config
            .commands
            .iter()
            .filter_map(|c| {
                c.shortcut
                    .as_deref()
                    .and_then(parse_shortcut)
                    .map(|s| (c.name.clone(), s))
            })
            .collect();
        for (name, sc) in cmds {
            if ctx.input_mut(|i| i.consume_shortcut(&sc)) {
                self.state.click_command(&name);
            }
        }
        let marker = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::M);
        if ctx.input_mut(|i| i.consume_shortcut(&marker)) {
            let t = std::mem::take(&mut self.marker_text);
            self.state.add_marker(&t);
        }
    }

    fn menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New project…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Folder for the new project")
                        .pick_folder()
                    {
                        let name = dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "New project".into());
                        match pb_core::project::create_project(&dir, &name) {
                            Ok(_) => self.open_project(dir),
                            Err(e) => self.state.notify(format!("cannot create project: {e}")),
                        }
                    }
                }
                if ui.button("Open project…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Open project folder")
                        .pick_folder()
                    {
                        self.open_project(dir);
                    }
                }
                ui.menu_button("Recent", |ui| {
                    let recent = self.state.recent.clone();
                    if recent.is_empty() {
                        ui.label("(none)");
                    }
                    for r in recent {
                        if ui.button(r.display().to_string()).clicked() {
                            self.open_project(r);
                        }
                    }
                });
                let has = self.state.project.is_some();
                if ui
                    .add_enabled(has, egui::Button::new("Duplicate project…"))
                    .clicked()
                {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("Copy project to (new folder)")
                        .save_file()
                    {
                        let name = dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if let Some(p) = &self.state.project {
                            match p.duplicate(&dir, &name) {
                                Ok(d) => self.open_project(d),
                                Err(e) => self.state.notify(format!("duplicate failed: {e}")),
                            }
                        }
                    }
                }
                if ui
                    .add_enabled(has, egui::Button::new("Reload project"))
                    .clicked()
                {
                    self.state.reload_project();
                    self.load_project_layout();
                }
                ui.separator();
                if ui.button("Settings…").clicked() {
                    self.windows.settings = true;
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Connection", |ui| {
                let profiles = self.state.profiles();
                if profiles.is_empty() {
                    ui.label("(no profiles)");
                }
                for p in &profiles {
                    let selected = self.state.profile.as_deref() == Some(p.name.as_str());
                    if ui
                        .selectable_label(selected, format!("{} — {}", p.name, p.endpoint()))
                        .clicked()
                    {
                        self.state.profile = Some(p.name.clone());
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("Port override:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.state.port_override)
                            .desired_width(140.0)
                            .hint_text("COM5 / host:port"),
                    );
                });
                ui.separator();
                let connected = self.state.session.is_some();
                if !connected {
                    if ui
                        .add_enabled(self.state.profile.is_some(), egui::Button::new("Connect"))
                        .clicked()
                    {
                        self.connect_selected();
                    }
                } else if ui.button("Disconnect").clicked() {
                    self.state.disconnect();
                }
                ui.separator();
                ui.add_enabled_ui(connected, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("DTR");
                        if ui.button("on").clicked() {
                            self.with_session(|s| s.set_dtr(true));
                        }
                        if ui.button("off").clicked() {
                            self.with_session(|s| s.set_dtr(false));
                        }
                        ui.label("RTS");
                        if ui.button("on").clicked() {
                            self.with_session(|s| s.set_rts(true));
                        }
                        if ui.button("off").clicked() {
                            self.with_session(|s| s.set_rts(false));
                        }
                    });
                    if ui.button("Pulse reset").clicked() {
                        self.with_session(|s| s.pulse_reset());
                    }
                });
            });
            ui.menu_button("Record", |ui| {
                let connected = self.state.session.is_some();
                if self.state.recording.is_none() {
                    if ui
                        .add_enabled(connected, egui::Button::new("Start recording"))
                        .clicked()
                    {
                        if let Err(e) = self.state.start_recording() {
                            self.state.notify(format!("cannot record: {e}"));
                        }
                    }
                } else if ui.button("Stop recording").clicked() {
                    self.state.stop_recording();
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.marker_text)
                            .desired_width(120.0)
                            .hint_text("marker label"),
                    );
                    if ui.button("Add marker (Ctrl+M)").clicked() {
                        let t = std::mem::take(&mut self.marker_text);
                        self.state.add_marker(&t);
                    }
                });
                ui.separator();
                if ui
                    .add_enabled(connected, egui::Button::new("Export CSV (joined)…"))
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("CSV", &["csv"])
                        .set_file_name("channels.csv")
                        .save_file()
                    {
                        self.state.export_csv(
                            path,
                            Vec::new(),
                            None,
                            pb_core::recorder::CsvMode::Joined,
                        );
                    }
                }
                if ui
                    .add_enabled(
                        connected,
                        egui::Button::new("Export CSV (one file per channel)…"),
                    )
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("CSV", &["csv"])
                        .set_file_name("channels.csv")
                        .save_file()
                    {
                        self.state.export_csv(
                            path,
                            Vec::new(),
                            None,
                            pb_core::recorder::CsvMode::PerChannel,
                        );
                    }
                }
                if ui
                    .add_enabled(connected, egui::Button::new("Export Parquet…"))
                    .on_hover_text("time_s plus one column per channel, joined like the CSV")
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Parquet", &["parquet"])
                        .set_file_name("channels.parquet")
                        .save_file()
                    {
                        self.state.export_parquet(path);
                    }
                }
                if ui.button("Export recording to MCAP…").clicked() {
                    let start = self.state.project.as_ref().map(|p| p.recordings_dir());
                    let mut d =
                        rfd::FileDialog::new().add_filter("ProtoBench session", &["pbsession"]);
                    if let Some(s) = start {
                        d = d.set_directory(s);
                    }
                    if let Some(rec) = d.pick_file() {
                        if let Some(out) = rfd::FileDialog::new()
                            .add_filter("MCAP", &["mcap"])
                            .set_file_name("session.mcap")
                            .save_file()
                        {
                            self.state.export_mcap(rec, out);
                        }
                    }
                }
            });
            ui.menu_button("View", |ui| {
                ui.menu_button("Add panel", |ui| {
                    for p in Panel::all() {
                        let label = match &p {
                            Panel::Table(c) if c.mode == crate::layout::TableMode::Log => {
                                "Table (message log)".to_string()
                            }
                            Panel::Table(_) => "Table (latest values)".to_string(),
                            other => other.title().to_string(),
                        };
                        if ui.button(label).clicked() {
                            self.actions.push(Action::AddPanel(p));
                        }
                    }
                });
                if ui
                    .button("Reset layout")
                    .on_hover_text("Back to the project's layout.json")
                    .clicked()
                {
                    self.actions.push(Action::ResetLayout);
                }
                if ui
                    .add_enabled(
                        self.state.project.is_some(),
                        egui::Button::new("Save layout as project default"),
                    )
                    .on_hover_text("Write the current layout to the project's layout.json")
                    .clicked()
                {
                    self.actions.push(Action::SaveProjectLayout);
                }
                if ui
                    .checkbox(&mut self.state.settings.dark, "Dark theme")
                    .changed()
                {
                    self.applied_dark = None;
                }
                if ui
                    .checkbox(&mut self.windows.errors, "Error panel")
                    .clicked()
                {}
            });
            ui.menu_button("Tools", |ui| {
                if ui.button("Schema browser").clicked() {
                    self.windows.schema = true;
                }
                if ui.button("Message composer").clicked() {
                    self.windows.composer = true;
                }
                if ui.button("Sequence runner").clicked() {
                    self.windows.runner = true;
                }
                if ui.button("Link statistics").clicked() {
                    self.actions.push(Action::ShowLinkStats);
                }
                if ui.button("Project editor").clicked() {
                    self.windows.editor = true;
                }
                ui.separator();
                let mut on = self.state.foxglove_enabled;
                let label = match self.state.foxglove_status() {
                    Some((addr, n)) => format!("Foxglove server (ws://{addr}, {n} clients)"),
                    None => "Foxglove server".to_string(),
                };
                if ui
                    .checkbox(&mut on, label)
                    .on_hover_text("Serve live data to Foxglove; port in Settings")
                    .changed()
                {
                    if let Err(e) = self.state.set_foxglove(on) {
                        self.state.notify(format!("Foxglove server: {e}"));
                    }
                }
                let triggers = self
                    .state
                    .session
                    .as_ref()
                    .map(|s| s.triggers())
                    .unwrap_or_default();
                if !triggers.is_empty() {
                    ui.menu_button("Arm triggered capture", |ui| {
                        for t in triggers {
                            let label = if t.remaining > 0 {
                                format!("{} ({} left)", t.name, t.remaining)
                            } else {
                                format!("{} ({} samples)", t.name, t.samples)
                            };
                            if ui.button(label).clicked() {
                                self.with_session(|s| s.arm_trigger(&t.name));
                            }
                        }
                    });
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About ProtoBench").clicked() {
                    self.windows.about = true;
                }
                ui.label("Ctrl+M: add marker");
                ui.label("Shift+click plot: cursor A, Ctrl+click: cursor B");
                ui.label("Docs: docs/USER_GUIDE.md");
            });
        });
    }

    fn with_session(&self, f: impl FnOnce(&pb_core::session::Session)) {
        if let Some(s) = &self.state.session {
            f(s);
        }
    }

    /// Connects with the selected profile.
    pub fn connect_selected(&mut self) {
        if let Some(p) = self.state.profile.clone() {
            if let Err(e) = self.state.connect(&p) {
                self.state.notify(format!("connect failed: {e}"));
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (text, color) = match &self.state.session {
                None => ("Disconnected".to_string(), Color32::GRAY),
                Some(s) => {
                    let st = s.state();
                    let c = match st {
                        pb_core::session::ConnState::Connected => {
                            Color32::from_rgb(0x2e, 0xcc, 0x71)
                        }
                        pb_core::session::ConnState::Failed(_) => {
                            Color32::from_rgb(0xe7, 0x4c, 0x3c)
                        }
                        _ => Color32::from_rgb(0xf3, 0x9c, 0x12),
                    };
                    (st.label(), c)
                }
            };
            ui.label(RichText::new("●").color(color));
            ui.label(text);
            if let Some(s) = &self.state.session {
                ui.separator();
                ui.label(s.endpoint());
                let st = s.stats();
                ui.separator();
                ui.label(format!(
                    "rx {:.0} msg/s {}  tx {:.0} frm/s {}",
                    st.rx_mps,
                    fmt_rate(st.rx_bps),
                    st.tx_fps,
                    fmt_rate(st.tx_bps)
                ));
                ui.separator();
                let errs = st.errors();
                let label = RichText::new(format!(
                    "errors: framing {} crc {} decode {} id {}",
                    st.framing_errors, st.crc_errors, st.decode_errors, st.unknown_ids
                ))
                .color(if errs > 0 {
                    Color32::from_rgb(0xe6, 0x7e, 0x22)
                } else {
                    ui.visuals().text_color()
                });
                if ui
                    .add(egui::Label::new(label).sense(egui::Sense::click()))
                    .on_hover_text("Open link statistics")
                    .clicked()
                {
                    self.actions.push(Action::ShowLinkStats);
                }
                if let Some(e) = s.recording_elapsed_s() {
                    ui.separator();
                    ui.label(RichText::new(format!("● REC {e:.1} s")).color(Color32::RED));
                }
            }
            if let Some((addr, n)) = self.state.foxglove_status() {
                ui.separator();
                ui.label(format!("Foxglove {addr} · {n}"))
                    .on_hover_text("Foxglove WebSocket server: address · connected clients");
            }
            if let Some((t, n)) = self.state.notifications.back() {
                if t.elapsed() < Duration::from_secs(8) {
                    ui.separator();
                    ui.label(RichText::new(n.lines().next().unwrap_or_default()).italics());
                }
            }
        });
    }

    fn error_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong(format!("Problems ({})", self.state.diagnostics.len()));
            if ui.small_button("Hide").clicked() {
                self.windows.errors = false;
            }
            if ui.small_button("Reload project").clicked() {
                self.state.reload_project();
                self.load_project_layout();
            }
        });
        egui::ScrollArea::vertical()
            .max_height(140.0)
            .show(ui, |ui| {
                if self.state.diagnostics.is_empty() {
                    ui.label("No problems.");
                }
                for d in &self.state.diagnostics {
                    let c = if d.is_error() {
                        Color32::from_rgb(0xe7, 0x4c, 0x3c)
                    } else {
                        Color32::from_rgb(0xf3, 0x9c, 0x12)
                    };
                    ui.label(RichText::new(d.to_string()).color(c).monospace());
                }
                if let Some(d) = &self.state.proto_diff {
                    ui.label(
                        RichText::new(format!("Schema changed:\n{d}"))
                            .color(Color32::from_rgb(0xf3, 0x9c, 0x12)),
                    );
                }
            });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(d) = self.state.dialog.clone() else {
            return;
        };
        let mut close = false;
        match d {
            Dialog::Prompt { command, .. } => {
                egui::Modal::new(egui::Id::new("prompt")).show(ctx, |ui| {
                    ui.heading(&command);
                    ui.label("Edit the values to send:");
                    if let Some(Dialog::Prompt { fields, .. }) = &mut self.state.dialog {
                        egui::Grid::new("prompt_grid").show(ui, |ui| {
                            for (k, v) in fields.iter_mut() {
                                ui.label(k.as_str());
                                ui.text_edit_singleline(v);
                                ui.end_row();
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Send").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            self.state.submit_prompt();
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Confirm { command, .. } => {
                egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
                    ui.heading(
                        RichText::new(format!("⚠ {command}"))
                            .color(Color32::from_rgb(0xe7, 0x4c, 0x3c)),
                    );
                    ui.label("This command is marked dangerous. Send it?");
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Send").color(Color32::WHITE))
                                    .fill(Color32::from_rgb(0xc0, 0x39, 0x2b)),
                            )
                            .clicked()
                        {
                            self.state.confirm();
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Operator { message, .. } => {
                egui::Modal::new(egui::Id::new("operator")).show(ctx, |ui| {
                    ui.heading("Operator action");
                    ui.label(&message);
                    let mut all_valid = true;
                    if let Some(Dialog::Operator { fields, .. }) = &mut self.state.dialog {
                        if !fields.is_empty() {
                            ui.add_space(4.0);
                            egui::Grid::new("operator_grid").show(ui, |ui| {
                                for f in fields.iter_mut() {
                                    operator_field(ui, f);
                                    if f.value().is_err() {
                                        all_valid = false;
                                    }
                                }
                            });
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(all_valid, egui::Button::new("OK")).clicked() {
                            self.state.answer_operator(true);
                        }
                        if ui.button("Fail").clicked() {
                            self.state.answer_operator(false);
                        }
                    });
                });
            }
            Dialog::Reload { what } => {
                egui::Modal::new(egui::Id::new("reload")).show(ctx, |ui| {
                    ui.heading("Project changed on disk");
                    ui.label(format!("Changed: {what}"));
                    ui.horizontal(|ui| {
                        if ui.button("Reload").clicked() {
                            self.state.dialog = None;
                            self.state.reload_project();
                            self.load_project_layout();
                            self.windows.errors =
                                self.state.diagnostics.iter().any(|d| d.is_error())
                                    || self.state.proto_diff.is_some();
                        }
                        if ui.button("Ignore").clicked() {
                            close = true;
                        }
                    });
                });
            }
        }
        if close {
            self.state.cancel_dialog();
        }
    }

    fn tool_windows(&mut self, ctx: &egui::Context) {
        if self.windows.schema {
            let mut open = true;
            egui::Window::new("Schema Browser")
                .open(&mut open)
                .default_size([520.0, 600.0])
                .show(ctx, |ui| {
                    crate::tools::schema_browser(ui, &mut self.schema_browser, &self.state);
                });
            self.windows.schema = open;
        }
        if self.windows.composer {
            let mut open = true;
            egui::Window::new("Message Composer")
                .open(&mut open)
                .default_size([480.0, 560.0])
                .show(ctx, |ui| {
                    crate::tools::composer(ui, &mut self.composer, &mut self.state);
                });
            self.windows.composer = open;
        }
        if self.windows.runner {
            let mut open = true;
            egui::Window::new("Sequence Runner")
                .open(&mut open)
                .default_size([560.0, 560.0])
                .show(ctx, |ui| {
                    crate::tools::runner(ui, &mut self.runner, &mut self.state);
                });
            self.windows.runner = open;
        }
        crate::tools::runner_poll(&mut self.runner, &mut self.state);
        if self.windows.settings {
            let mut open = true;
            egui::Window::new("Settings")
                .open(&mut open)
                .show(ctx, |ui| {
                    crate::tools::settings(ui, &mut self.state.settings);
                });
            self.windows.settings = open;
            self.applied_dark = None;
        }
        if self.windows.editor {
            let mut open = true;
            let mut reload = false;
            egui::Window::new("Project Editor")
                .open(&mut open)
                .default_size([520.0, 480.0])
                .show(ctx, |ui| {
                    reload = crate::tools::project_editor(ui, &mut self.editor, &mut self.state);
                });
            if reload {
                self.load_project_layout();
            }
            self.windows.editor = open;
        }
        if self.windows.about {
            let mut open = true;
            egui::Window::new("About ProtoBench")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.heading(format!("ProtoBench {}", pb_core::VERSION));
                    ui.label("Develop and test embedded devices that speak Protocol Buffers.");
                    ui.label("Schemas compile at runtime with protox; no protoc needed.");
                });
            self.windows.about = open;
        }
    }

    /// Draws one frame. Split out so tests and the smoke test can drive it.
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some(mut hook) = self.hook.take() {
            hook(self, &ctx);
            self.hook = Some(hook);
        }
        if self.applied_dark != Some(self.state.settings.dark) {
            ctx.set_visuals(if self.state.settings.dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
            self.applied_dark = Some(self.state.settings.dark);
        }
        self.state.poll();
        self.state.check_files();
        self.shortcuts(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu(ui, &ctx));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.windows.errors {
            egui::Panel::bottom("errors")
                .resizable(true)
                .show(ui, |ui| self.error_panel(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if self.state.project.is_none() {
                welcome(ui, self);
                return;
            }
            let mut viewer = Viewer {
                state: &mut self.state,
                actions: &mut self.actions,
            };
            DockArea::new(&mut self.dock)
                .style(Style::from_egui(ui.style().as_ref()))
                .show_add_buttons(false)
                .show_inside(ui, &mut viewer);
        });
        self.handle_actions();
        self.tool_windows(&ctx);
        self.dialogs(&ctx);

        if self.last_layout_check.elapsed() > Duration::from_secs(2) {
            self.last_layout_check = Instant::now();
            self.save_layout_now();
        }
        if self.state.session.is_some() || self.runner.is_running() {
            let fps = self.state.settings.fps_cap.clamp(1, 240);
            ctx.request_repaint_after(Duration::from_secs_f64(1.0 / f64::from(fps)));
        } else {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

fn welcome(ui: &mut egui::Ui, app: &mut ProtoBenchApp) {
    ui.vertical_centered(|ui| {
        ui.add_space(60.0);
        ui.heading("ProtoBench");
        ui.label("Open a project folder (project.toml + .proto files) to start.");
        ui.add_space(12.0);
        if ui.button("Open project…").clicked() {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                app.open_project(dir);
                return;
            }
        }
        ui.add_space(12.0);
        for r in app.state.recent.clone() {
            if ui.link(r.display().to_string()).clicked() {
                app.open_project(r);
                return;
            }
        }
    });
}

impl eframe::App for ProtoBenchApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_SETTINGS, &self.state.settings);
        eframe::set_value(storage, STORAGE_RECENT, &self.state.recent);
        self.save_layout_now();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_layout_now();
        self.state.disconnect();
    }
}

fn layout_fingerprint(dock: &DockState<Tab>) -> String {
    // Tabs and tree shape; rects change every frame and are ignored.
    let tabs: Vec<_> = dock
        .iter_all_tabs()
        .map(|(p, t)| (format!("{p:?}"), t.clone()))
        .collect();
    serde_json::to_string(&tabs.iter().map(|(p, t)| (p, t)).collect::<Vec<_>>()).unwrap_or_default()
}

fn fmt_rate(bps: f64) -> String {
    if bps >= 1e6 {
        format!("{:.1} MB/s", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.1} kB/s", bps / 1e3)
    } else {
        format!("{bps:.0} B/s")
    }
}

/// Parses `F1`, `Ctrl+S`, `Shift+Alt+3`.
pub fn parse_shortcut(s: &str) -> Option<egui::KeyboardShortcut> {
    let mut mods = egui::Modifiers::NONE;
    let mut key = None;
    for part in s.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "cmd" => mods |= egui::Modifiers::COMMAND,
            "shift" => mods |= egui::Modifiers::SHIFT,
            "alt" => mods |= egui::Modifiers::ALT,
            _ => {
                key = egui::Key::from_name(part)
                    .or_else(|| egui::Key::from_name(&part.to_ascii_uppercase()))
            }
        }
    }
    key.map(|k| egui::KeyboardShortcut::new(mods, k))
}

/// One row of the operator dialog: label, field, unit and why it is invalid.
fn operator_field(ui: &mut egui::Ui, f: &mut crate::state::OperatorField) {
    use pb_core::sequence::InputType;
    ui.label(f.input.label());
    match (f.input.ty, &f.input.choices) {
        (InputType::Bool, _) => {
            let mut b = f.text.trim() == "true";
            if ui.checkbox(&mut b, "").changed() || f.text.trim().is_empty() {
                f.text = b.to_string();
            }
        }
        (InputType::String, Some(choices)) => {
            egui::ComboBox::from_id_salt(("operator_choice", &f.input.var))
                .selected_text(f.text.as_str())
                .show_ui(ui, |ui| {
                    for c in choices {
                        ui.selectable_value(&mut f.text, c.clone(), c);
                    }
                });
        }
        _ => {
            ui.add(egui::TextEdit::singleline(&mut f.text).desired_width(140.0));
        }
    }
    ui.label(&f.input.unit);
    match f.value() {
        Ok(_) => ui.label(""),
        Err(e) => ui.label(RichText::new(e).color(Color32::from_rgb(0xe7, 0x4c, 0x3c))),
    };
    ui.end_row();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rearranging_writes_only_the_local_layout() {
        let src = pb_core::project::Project::open(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor"),
        );
        let dir = std::env::temp_dir().join(format!("pb-app-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        src.duplicate(&dir, "Copy").unwrap();
        let project_layout = std::fs::read_to_string(dir.join("layout.json")).unwrap();

        let mut app = ProtoBenchApp::new(None, Some(dir.clone()), None);
        app.actions.push(Action::AddPanel(Panel::Inspector));
        app.handle_actions();
        app.save_layout_now();
        let local = dir.join(crate::layout::LOCAL_LAYOUT_FILE);
        assert!(local.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("layout.json")).unwrap(),
            project_layout
        );

        // Reopening restores the working layout; Reset goes back to layout.json.
        let tabs = app.dock.iter_all_tabs().count();
        let mut app = ProtoBenchApp::new(None, Some(dir.clone()), None);
        assert_eq!(app.dock.iter_all_tabs().count(), tabs);
        app.actions.push(Action::ResetLayout);
        app.handle_actions();
        assert_eq!(app.dock.iter_all_tabs().count(), tabs - 1);

        // Only the explicit action writes layout.json.
        app.actions.push(Action::AddPanel(Panel::Markers));
        app.actions.push(Action::SaveProjectLayout);
        app.handle_actions();
        assert_ne!(
            std::fs::read_to_string(dir.join("layout.json")).unwrap(),
            project_layout
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shortcuts_parse() {
        let s = parse_shortcut("F1").unwrap();
        assert_eq!(s.logical_key, egui::Key::F1);
        let s = parse_shortcut("Ctrl+S").unwrap();
        assert_eq!(s.logical_key, egui::Key::S);
        assert!(s.modifiers.command);
        assert!(parse_shortcut("Ctrl+Nope").is_none());
    }
}

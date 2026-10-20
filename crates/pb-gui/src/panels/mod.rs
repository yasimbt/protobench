//! Panel rendering. Each panel reads [`AppState`] and calls its actions.

mod consoles;
mod plots;
mod tables;
mod widgets;

use egui::{Color32, RichText, WidgetText};

use crate::app::Action;
use crate::layout::{Panel, Tab};
use crate::state::{AppState, Badge};

/// Payload dragged from the channel list onto plots.
#[derive(Debug, Clone)]
pub struct ChannelDrag(pub String);

/// The dock's tab viewer.
pub struct Viewer<'a> {
    /// State.
    pub state: &'a mut AppState,
    /// Deferred actions.
    pub actions: &'a mut Vec<Action>,
}

impl egui_dock::TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(("tab", tab.id))
    }

    fn title(&mut self, tab: &mut Tab) -> WidgetText {
        tab.title.clone().into()
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        ui.label("Title:");
        ui.text_edit_singleline(&mut tab.title);
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        let id = tab.id;
        match &mut tab.panel {
            Panel::TimePlot(cfg) => plots::time_plot(ui, self.state, cfg, id),
            Panel::XyPlot(cfg) => plots::xy_plot(ui, self.state, cfg, id),
            Panel::Histogram(cfg) => plots::histogram(ui, self.state, cfg, id),
            Panel::Readouts(cfg) => widgets::readouts(ui, self.state, cfg),
            Panel::Indicators(cfg) => widgets::indicators(ui, self.state, cfg),
            Panel::ChannelStats => widgets::channel_stats(ui, self.state),
            Panel::LinkStats => widgets::link_stats(ui, self.state),
            Panel::Channels => widgets::channel_list(ui, self.state, self.actions),
            Panel::CommandBar => command_bar(ui, self.state),
            Panel::History => history(ui, self.state),
            Panel::Markers => markers(ui, self.state),
            Panel::Table(cfg) => tables::table(ui, self.state, cfg, id),
            Panel::Inspector => tables::inspector(ui, self.state),
            Panel::RawConsole(cfg) => consoles::raw_console(ui, self.state, cfg, id),
            Panel::TextConsole(cfg) => consoles::text_console(ui, self.state, cfg, id),
        }
    }
}

/// Colour of a `#rrggbb` string.
pub fn colour(s: &str) -> Option<Color32> {
    pb_core::config::parse_colour(s).map(|[r, g, b]| Color32::from_rgb(r, g, b))
}

/// Green / amber / red used across panels.
pub const OK: Color32 = Color32::from_rgb(0x2e, 0xcc, 0x71);
/// Warning colour.
pub const WARN: Color32 = Color32::from_rgb(0xf3, 0x9c, 0x12);
/// Error colour.
pub const BAD: Color32 = Color32::from_rgb(0xe7, 0x4c, 0x3c);

fn not_connected(ui: &mut egui::Ui) {
    ui.weak("Not connected. Use Connection ▸ Connect.");
}

/// Command buttons grouped by `group`.
pub fn command_bar(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(p) = &state.project else { return };
    let groups: Vec<(String, Vec<pb_core::config::CommandConfig>)> =
        pb_core::commands::group_commands(&p.config.commands)
            .into_iter()
            .map(|(g, v)| (g, v.into_iter().cloned().collect()))
            .collect();
    if groups.is_empty() {
        ui.weak("No [[commands]] in project.toml.");
    }
    let connected = state.session.is_some();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (group, cmds) in groups {
            egui::CollapsingHeader::new(RichText::new(&group).strong())
                .default_open(true)
                .show(ui, |ui| {
                    for c in cmds {
                        ui.horizontal(|ui| {
                            let mut label = String::new();
                            if c.dangerous {
                                label.push_str("⚠ ");
                            }
                            label.push_str(&c.name);
                            if !c.prompt.is_empty() {
                                label.push('…');
                            }
                            let mut text = RichText::new(label);
                            let mut button = egui::Button::new(text.clone());
                            if let Some(col) = c.colour.as_deref().and_then(colour) {
                                text = text.color(Color32::WHITE);
                                button = egui::Button::new(text).fill(col);
                            }
                            if let Some(sc) = &c.shortcut {
                                button = button.shortcut_text(sc.as_str());
                            }
                            let resp = ui.add_enabled(connected, button);
                            let resp = match &c.expect {
                                Some(e) => {
                                    resp.on_hover_text(format!("{} → expects {e}", c.message))
                                }
                                None => {
                                    resp.on_hover_text(format!("{} (no reply expected)", c.message))
                                }
                            };
                            if resp.clicked() {
                                state.click_command(&c.name);
                            }
                            badge(ui, state.badges.get(&c.name));
                            if let Some(ms) = c.periodic_ms {
                                let mut on = state.is_periodic(&c.name);
                                if ui
                                    .add_enabled(
                                        connected,
                                        egui::Checkbox::new(&mut on, format!("every {ms} ms")),
                                    )
                                    .changed()
                                {
                                    if let Err(e) = state.set_periodic(&c.name, on) {
                                        state.notify(e);
                                    }
                                }
                            }
                        });
                    }
                });
        }
    });
}

fn badge(ui: &mut egui::Ui, b: Option<&Badge>) {
    match b {
        None => {}
        Some(Badge::Pending) => {
            ui.spinner();
        }
        Some(Badge::Sent) => {
            ui.label(RichText::new("✔ sent").color(OK))
                .on_hover_text("written (no reply expected)");
        }
        Some(Badge::Ok { latency_ms }) => {
            ui.label(RichText::new("✔").color(OK).strong())
                .on_hover_text(format!("reply in {latency_ms:.1} ms"));
        }
        Some(Badge::Timeout) => {
            ui.label(RichText::new("⏱ timeout").color(WARN))
                .on_hover_text("no reply within the timeout");
        }
        Some(Badge::Error(e)) => {
            ui.label(RichText::new("✖ error").color(BAD))
                .on_hover_text(e.as_str());
        }
    }
}

/// Sent-command history with resend.
pub fn history(ui: &mut egui::Ui, state: &mut AppState) {
    if state.history.is_empty() {
        ui.weak("No commands sent yet.");
        return;
    }
    let entries: Vec<_> = state.history.iter().cloned().enumerate().collect();
    let mut resend = None;
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for (i, e) in entries.iter().rev() {
                ui.horizontal(|ui| {
                    if ui.small_button("↻").on_hover_text("Resend").clicked() {
                        resend = Some(*i);
                    }
                    ui.monospace(format!("{:9.3}", e.host_ts_ns as f64 / 1e9));
                    ui.strong(&e.name);
                    ui.label(RichText::new(e.payload.to_string()).monospace().weak());
                });
            }
        });
    if let Some(i) = resend {
        if let Err(e) = state.resend(i) {
            state.notify(e);
        }
    }
}

/// Markers and session notes.
pub fn markers(ui: &mut egui::Ui, state: &mut AppState) {
    let id = ui.id().with("marker_text");
    let mut text: String = ui.data_mut(|d| d.get_temp(id).unwrap_or_default());
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut text)
                .hint_text("label")
                .desired_width(160.0),
        );
        if ui.button("Add marker").clicked()
            || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
        {
            state.add_marker(&text);
            text.clear();
        }
    });
    ui.data_mut(|d| d.insert_temp(id, text));
    ui.label("Notes (added to the recording as a marker when you press Save note):");
    ui.add(
        egui::TextEdit::multiline(&mut state.notes)
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    if ui.button("Save note").clicked() && !state.notes.trim().is_empty() {
        let n = format!("note: {}", state.notes.trim());
        state.add_marker(&n);
        state.notes.clear();
    }
    ui.separator();
    for m in state.markers.iter().rev() {
        ui.horizontal(|ui| {
            ui.monospace(format!("{:9.3}", m.t));
            ui.label(&m.text);
        });
    }
}

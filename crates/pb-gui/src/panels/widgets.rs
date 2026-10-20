//! Readouts, indicators, channel list and statistics panels.

use std::sync::Arc;

use egui::{Color32, RichText};

use pb_core::schema::LimitState;
use pb_core::telemetry::{Channel, ChannelKind};

use super::{not_connected, ChannelDrag, BAD, OK, WARN};
use crate::layout::{ChannelsCfg, PALETTE};
use crate::state::AppState;

fn channels(state: &AppState) -> Vec<Arc<Channel>> {
    state
        .session
        .as_ref()
        .map(|s| s.telemetry().channels())
        .unwrap_or_default()
}

fn pick(
    cfg: &ChannelsCfg,
    all: &[Arc<Channel>],
    want: impl Fn(&Channel) -> bool,
) -> Vec<Arc<Channel>> {
    if cfg.channels.is_empty() {
        all.iter().filter(|c| want(c)).cloned().collect()
    } else {
        cfg.channels
            .iter()
            .filter_map(|n| all.iter().find(|c| &c.name == n).cloned())
            .collect()
    }
}

fn channel_menu(ui: &mut egui::Ui, cfg: &mut ChannelsCfg, all: &[Arc<Channel>]) {
    ui.menu_button("⚙ channels", |ui| {
        if ui.button("Show all").clicked() {
            cfg.channels.clear();
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(ui, |ui| {
                for c in all {
                    let mut on = cfg.channels.contains(&c.name);
                    if ui.checkbox(&mut on, &c.name).changed() {
                        if on {
                            cfg.channels.push(c.name.clone());
                        } else {
                            cfg.channels.retain(|n| n != &c.name);
                        }
                    }
                }
            });
    });
}

/// Numeric tiles with limit colouring.
pub fn readouts(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut ChannelsCfg) {
    if state.session.is_none() {
        not_connected(ui);
        return;
    }
    let all = channels(state);
    channel_menu(ui, cfg, &all);
    let shown = pick(cfg, &all, |c| {
        matches!(c.kind, ChannelKind::Number | ChannelKind::Derived)
    });
    if shown.is_empty() {
        ui.weak("Waiting for data…");
        return;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            for c in shown {
                let last = c.last();
                let (text, fill) = match last {
                    Some((_, v)) => {
                        let fill = match c.display.limit_state(v) {
                            LimitState::Normal => None,
                            LimitState::Warn => Some(WARN),
                            LimitState::Alarm => Some(BAD),
                        };
                        (c.format_value(v), fill)
                    }
                    None => ("—".into(), None),
                };
                let mut frame = egui::Frame::group(ui.style()).inner_margin(8.0);
                if let Some(f) = fill {
                    frame = frame.fill(f.gamma_multiply(0.6));
                }
                frame
                    .show(ui, |ui| {
                        ui.set_min_width(130.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&c.display.display).small());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(text).size(24.0).strong().monospace());
                                if !c.display.unit.is_empty() {
                                    ui.label(&c.display.unit);
                                }
                            });
                        });
                    })
                    .response
                    .on_hover_text(&c.name);
            }
        });
    });
}

/// LEDs for enum and bool channels.
pub fn indicators(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut ChannelsCfg) {
    if state.session.is_none() {
        not_connected(ui);
        return;
    }
    let all = channels(state);
    channel_menu(ui, cfg, &all);
    let shown = pick(cfg, &all, |c| {
        matches!(c.kind, ChannelKind::Enum(_) | ChannelKind::Bool)
    });
    if shown.is_empty() {
        ui.weak("No enum or bool channels yet.");
        return;
    }
    for c in shown {
        let v = c.last().map(|(_, v)| v);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&c.display.display).strong());
            match &c.kind {
                ChannelKind::Bool => {
                    led(
                        ui,
                        v.map(|v| if v != 0.0 { OK } else { Color32::DARK_GRAY }),
                    );
                    ui.label(v.map(|v| c.format_value(v)).unwrap_or_else(|| "—".into()));
                }
                ChannelKind::Enum(e) => {
                    for (i, ev) in e.values().enumerate() {
                        let active = v.is_some_and(|v| v as i32 == ev.number());
                        let col = if active {
                            let [r, g, b] = PALETTE[i % PALETTE.len()];
                            let name = ev.name().to_ascii_uppercase();
                            if name.contains("FAULT") || name.contains("ERR") {
                                BAD
                            } else {
                                Color32::from_rgb(r, g, b)
                            }
                        } else {
                            Color32::from_gray(60)
                        };
                        led(ui, Some(col));
                        let t = RichText::new(ev.name());
                        ui.label(if active { t.strong() } else { t.weak() });
                    }
                }
                _ => {
                    ui.label(v.map(|v| c.format_value(v)).unwrap_or_default());
                }
            }
        });
    }
}

fn led(ui: &mut egui::Ui, color: Option<Color32>) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let c = color.unwrap_or(Color32::from_gray(60));
    ui.painter().circle_filled(r.center(), 6.0, c);
    ui.painter()
        .circle_stroke(r.center(), 6.0, egui::Stroke::new(1.0, Color32::BLACK));
}

/// Draggable channel list.
pub fn channel_list(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut Vec<crate::app::Action>,
) {
    if state.session.is_none() {
        not_connected(ui);
        return;
    }
    let all = channels(state);
    if all.is_empty() {
        ui.weak("Channels appear when telemetry arrives (try Start Stream).");
        return;
    }
    ui.weak("Drag a channel onto a plot.");
    egui::ScrollArea::vertical().show(ui, |ui| {
        for c in all {
            let id = ui.id().with(("drag", &c.name));
            ui.horizontal(|ui| {
                if ui
                    .small_button("📈")
                    .on_hover_text("Open in a new time plot")
                    .clicked()
                {
                    let mut cfg = crate::layout::TimePlotCfg::default();
                    cfg.add_channel(&c.name);
                    actions.push(crate::app::Action::AddPanel(
                        crate::layout::Panel::TimePlot(cfg),
                    ));
                }
                ui.dnd_drag_source(id, ChannelDrag(c.name.clone()), |ui| {
                    let last = c.last().map(|(_, v)| c.format_value(v)).unwrap_or_default();
                    ui.label(format!("⠿ {}  {}", c.label(), last));
                })
                .response
                .on_hover_text(&c.name);
            });
        }
    });
}

/// Per-channel statistics table.
pub fn channel_stats(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(s) = &state.session else {
        not_connected(ui);
        return;
    };
    if ui.button("Reset statistics").clicked() {
        s.telemetry().reset_stats();
    }
    let all = s.telemetry().channels();
    egui::ScrollArea::both().show(ui, |ui| {
        egui::Grid::new("chstats")
            .striped(true)
            .num_columns(8)
            .show(ui, |ui| {
                for h in [
                    "channel", "rate Hz", "min", "max", "mean", "std", "last", "count",
                ] {
                    ui.strong(h);
                }
                ui.end_row();
                for c in all {
                    let st = c.stats();
                    ui.label(c.label()).on_hover_text(&c.name);
                    ui.monospace(format!("{:.1}", st.rate_hz));
                    for v in [st.min, st.max, st.mean] {
                        ui.monospace(c.display.format(v));
                    }
                    ui.monospace(format!("{:.4}", st.std));
                    ui.monospace(c.format_value(st.last));
                    ui.monospace(st.count.to_string());
                    ui.end_row();
                }
            });
    });
}

/// Link counters, bad frames and latency.
pub fn link_stats(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(s) = &state.session else {
        not_connected(ui);
        return;
    };
    let st = s.stats();
    let shared = s.shared().clone();
    ui.horizontal(|ui| {
        if ui.button("Reset counters").clicked() {
            shared.counters.reset();
            shared.bad_frames.clear();
            shared.latency.clear();
        }
    });
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("link").striped(true).show(ui, |ui| {
            let rows: [(&str, String); 17] = [
                ("rx bytes/s", format!("{:.0}", st.rx_bps)),
                ("tx bytes/s", format!("{:.0}", st.tx_bps)),
                ("rx messages/s", format!("{:.1}", st.rx_mps)),
                ("tx frames/s", format!("{:.1}", st.tx_fps)),
                ("rx bytes", st.rx_bytes.to_string()),
                ("tx bytes", st.tx_bytes.to_string()),
                ("rx frames", st.rx_frames.to_string()),
                ("tx frames", st.tx_frames.to_string()),
                ("framing errors", st.framing_errors.to_string()),
                ("CRC errors", st.crc_errors.to_string()),
                ("decode errors", st.decode_errors.to_string()),
                ("unknown message IDs", st.unknown_ids.to_string()),
                ("text lines", st.text_lines.to_string()),
                ("dropped events", st.dropped_events.to_string()),
                ("timeouts", st.timeouts.to_string()),
                ("retries", st.retries.to_string()),
                ("reconnects", st.reconnects.to_string()),
            ];
            for (k, v) in rows {
                ui.label(k);
                let bad = (k.contains("error") || k.contains("unknown") || k.contains("dropped"))
                    && v != "0";
                ui.label(if bad {
                    RichText::new(v).color(WARN).monospace()
                } else {
                    RichText::new(v).monospace()
                });
                ui.end_row();
            }
        });
        ui.separator();
        ui.strong("Request latency");
        let lat = shared.latency.list();
        if lat.is_empty() {
            ui.weak("No replies yet.");
        } else {
            egui::Grid::new("latency").striped(true).show(ui, |ui| {
                for h in [
                    "command", "count", "min ms", "avg ms", "max ms", "p95 ms", "timeouts",
                ] {
                    ui.strong(h);
                }
                ui.end_row();
                for (name, l) in lat {
                    ui.label(name);
                    ui.monospace(l.count.to_string());
                    ui.monospace(format!("{:.2}", l.min_ms));
                    ui.monospace(format!("{:.2}", l.avg_ms));
                    ui.monospace(format!("{:.2}", l.max_ms));
                    ui.monospace(format!("{:.2}", l.p95_ms));
                    ui.monospace(l.timeouts.to_string());
                    ui.end_row();
                }
            });
        }
        ui.separator();
        ui.strong("Last bad frames");
        let bad = shared.bad_frames.list();
        if bad.is_empty() {
            ui.weak("None.");
        }
        for b in bad.iter().rev() {
            ui.label(
                RichText::new(format!(
                    "{:9.3} {} — {}",
                    b.host_ts_ns as f64 / 1e9,
                    b.kind,
                    b.reason
                ))
                .color(WARN),
            );
            ui.label(
                RichText::new(pb_core::util::hex(&b.bytes))
                    .monospace()
                    .small(),
            );
        }
    });
}

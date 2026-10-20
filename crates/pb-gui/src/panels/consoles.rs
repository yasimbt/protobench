//! Raw byte console and firmware text console.

use egui::{Color32, RichText};

use pb_core::session::Direction;

use super::{not_connected, BAD, OK, WARN};
use crate::layout::{RawCfg, TextCfg};
use crate::state::{AppState, Level, RawEntry};

const RX: Color32 = Color32::from_rgb(0x2e, 0xcc, 0x71);
const TX: Color32 = Color32::from_rgb(0x34, 0x98, 0xdb);

fn ascii(b: &[u8]) -> String {
    b.iter()
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                '·'
            }
        })
        .collect()
}

fn matches_search(e: &RawEntry, needle: &str, needle_hex: &Option<Vec<u8>>) -> bool {
    if needle.is_empty() {
        return true;
    }
    if let Some(h) = needle_hex {
        if !h.is_empty() && e.bytes.windows(h.len()).any(|w| w == h.as_slice()) {
            return true;
        }
    }
    String::from_utf8_lossy(&e.bytes).contains(needle)
}

/// Hex + ASCII view of raw traffic, with a send box.
pub fn raw_console(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut RawCfg, tab_id: u64) {
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut cfg.hex, "hex");
        ui.checkbox(&mut cfg.ascii, "ASCII");
        let paused = cfg.paused_at.is_some();
        if ui
            .button(if paused { "▶ Resume" } else { "⏸ Pause" })
            .clicked()
        {
            cfg.paused_at = if paused {
                None
            } else {
                Some(state.raw_log.len())
            };
        }
        if ui.button("Clear").clicked() {
            state.raw_log.clear();
            cfg.paused_at = None;
        }
        ui.label("search");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.search)
                .desired_width(140.0)
                .hint_text("text or hex"),
        );
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut cfg.send_hex, "hex");
        let r = ui.add(
            egui::TextEdit::singleline(&mut cfg.input)
                .desired_width(260.0)
                .hint_text(if cfg.send_hex {
                    "AA 55 01"
                } else {
                    "text (\\n for newline)"
                }),
        );
        let send = ui
            .add_enabled(state.session.is_some(), egui::Button::new("Send raw"))
            .clicked()
            || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if send {
            let bytes = if cfg.send_hex {
                pb_core::util::parse_hex(&cfg.input)
            } else {
                Ok(cfg
                    .input
                    .replace("\\n", "\n")
                    .replace("\\r", "\r")
                    .into_bytes())
            };
            match (bytes, &state.session) {
                (Ok(b), Some(s)) => s.send_raw(b),
                (Err(e), _) => state.notify(format!("raw send: {e}")),
                _ => {}
            }
        }
    });
    if state.session.is_none() && state.raw_log.is_empty() {
        not_connected(ui);
        return;
    }
    let needle_hex = pb_core::util::parse_hex(&cfg.search).ok();
    let limit = cfg
        .paused_at
        .unwrap_or(state.raw_log.len())
        .min(state.raw_log.len());
    let rows: Vec<&RawEntry> = state
        .raw_log
        .iter()
        .take(limit)
        .filter(|e| matches_search(e, &cfg.search, &needle_hex))
        .collect();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt(("raw", tab_id))
        .stick_to_bottom(cfg.paused_at.is_none())
        .auto_shrink(false)
        .show_rows(ui, row_h, rows.len(), |ui, range| {
            for e in &rows[range] {
                let (tag, col) = match e.dir {
                    Direction::Rx => ("RX", RX),
                    Direction::Tx => ("TX", TX),
                };
                let mut s = format!("{:10.4} {tag} ", e.t);
                if cfg.hex {
                    s.push_str(&pb_core::util::hex(&e.bytes));
                    s.push_str("  ");
                }
                if cfg.ascii {
                    s.push_str(&ascii(&e.bytes));
                }
                ui.label(RichText::new(s).monospace().color(col));
            }
        });
}

/// Firmware log lines with level colours.
pub fn text_console(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut TextCfg, tab_id: u64) {
    ui.horizontal(|ui| {
        ui.label("filter");
        ui.add(egui::TextEdit::singleline(&mut cfg.filter).desired_width(160.0));
        ui.checkbox(&mut cfg.autoscroll, "auto-scroll");
        if ui.button("Clear").clicked() {
            state.text_log.clear();
        }
    });
    if state.text_log.is_empty() {
        ui.weak(if state.session.is_some() {
            "No text lines yet (text passthrough shows printable lines between frames)."
        } else {
            "Not connected."
        });
        return;
    }
    let f = cfg.filter.to_lowercase();
    let lines: Vec<_> = state
        .text_log
        .iter()
        .filter(|l| f.is_empty() || l.line.to_lowercase().contains(&f))
        .collect();
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::vertical()
        .id_salt(("text", tab_id))
        .stick_to_bottom(cfg.autoscroll)
        .auto_shrink(false)
        .show_rows(ui, row_h, lines.len(), |ui, range| {
            for l in &lines[range] {
                let col = match l.level {
                    Level::Error => BAD,
                    Level::Warn => WARN,
                    Level::Info => OK,
                    Level::Debug => Color32::GRAY,
                    Level::Plain => ui.visuals().text_color(),
                };
                ui.label(
                    RichText::new(format!("{:10.3}  {}", l.t, l.line))
                        .monospace()
                        .color(col),
                );
            }
        });
}

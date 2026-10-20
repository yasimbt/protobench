//! Table (latest values / message log) and message inspector.

use egui::text::LayoutJob;
use egui::{Color32, RichText, TextFormat};
use prost_reflect::{DynamicMessage, Kind, ReflectMessage, Value};

use pb_core::schema::wire::{field_spans, find_span};
use pb_core::session::Direction;

use super::{not_connected, OK};
use crate::layout::{TableCfg, TableMode};
use crate::state::AppState;

fn scalar_text(v: &Value, kind: &Kind) -> String {
    match (v, kind) {
        (Value::EnumNumber(n), Kind::Enum(e)) => e
            .get_value(*n)
            .map(|ev| format!("{} ({n})", ev.name()))
            .unwrap_or_else(|| n.to_string()),
        (Value::Bytes(b), _) => format!("[{}] {}", b.len(), pb_core::util::hex(b)),
        (Value::String(s), _) => format!("{s:?}"),
        (Value::F32(f), _) => format!("{f}"),
        (Value::F64(f), _) => format!("{f}"),
        (Value::Bool(b), _) => b.to_string(),
        (Value::I32(x), _) => x.to_string(),
        (Value::I64(x), _) => x.to_string(),
        (Value::U32(x), _) => x.to_string(),
        (Value::U64(x), _) => x.to_string(),
        (Value::Message(_), _) => "{…}".into(),
        (Value::List(l), _) => format!("[{} items]", l.len()),
        (Value::Map(m), _) => format!("{{{} entries}}", m.len()),
        (Value::EnumNumber(n), _) => n.to_string(),
    }
}

/// Draws a message tree. With `select`, clicking a field stores its path.
fn tree(
    ui: &mut egui::Ui,
    msg: &DynamicMessage,
    path: &mut Vec<String>,
    select: Option<&mut Option<Vec<String>>>,
    id: egui::Id,
) {
    let mut select = select;
    for (f, v) in msg.fields() {
        path.push(f.name().to_string());
        let label = format!("{} #{}", f.name(), f.number());
        let selected = select
            .as_ref()
            .is_some_and(|s| s.as_deref() == Some(path.as_slice()));
        match v {
            Value::Message(m) => {
                let r = egui::CollapsingHeader::new(RichText::new(label).strong())
                    .id_salt(id.with(path.join(".")))
                    .default_open(true)
                    .show(ui, |ui| tree(ui, m, path, select.as_deref_mut(), id));
                if r.header_response.clicked() {
                    if let Some(s) = select.as_deref_mut() {
                        *s = Some(path.clone());
                    }
                }
            }
            Value::List(items) if items.iter().any(|i| matches!(i, Value::Message(_))) => {
                egui::CollapsingHeader::new(format!("{label} [{}]", items.len()))
                    .id_salt(id.with(path.join(".")))
                    .show(ui, |ui| {
                        for (i, it) in items.iter().enumerate() {
                            if let Value::Message(m) = it {
                                egui::CollapsingHeader::new(format!("[{i}]"))
                                    .id_salt(id.with((path.join("."), i)))
                                    .show(ui, |ui| tree(ui, m, path, select.as_deref_mut(), id));
                            }
                        }
                    });
            }
            Value::List(items) => {
                let kind = f.kind();
                let s = items
                    .iter()
                    .map(|i| scalar_text(i, &kind))
                    .collect::<Vec<_>>()
                    .join(", ");
                if field_row(ui, &format!("{label} [{}]", items.len()), &s, selected) {
                    if let Some(sel) = select.as_deref_mut() {
                        *sel = Some(path.clone());
                    }
                }
            }
            other => {
                if field_row(ui, &label, &scalar_text(other, &f.kind()), selected) {
                    if let Some(sel) = select.as_deref_mut() {
                        *sel = Some(path.clone());
                    }
                }
            }
        }
        path.pop();
    }
}

fn field_row(ui: &mut egui::Ui, label: &str, value: &str, selected: bool) -> bool {
    ui.horizontal(|ui| {
        let r = ui.selectable_label(selected, RichText::new(label).monospace());
        ui.label(RichText::new(value).monospace());
        r.clicked()
    })
    .inner
}

/// Latest-values tree or scrolling message log.
pub fn table(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut TableCfg, tab_id: u64) {
    ui.horizontal(|ui| {
        ui.selectable_value(&mut cfg.mode, TableMode::Latest, "Latest values");
        ui.selectable_value(&mut cfg.mode, TableMode::Log, "Message log");
        ui.label("filter");
        ui.add(
            egui::TextEdit::singleline(&mut cfg.filter)
                .desired_width(120.0)
                .hint_text("type"),
        );
    });
    if state.session.is_none() && state.msg_log.is_empty() {
        not_connected(ui);
        return;
    }
    let f = cfg.filter.to_lowercase();
    let pass = |kind: &str, ty: &str| {
        f.is_empty() || kind.to_lowercase().contains(&f) || ty.to_lowercase().contains(&f)
    };
    match cfg.mode {
        TableMode::Latest => {
            let mut seen: Vec<String> = Vec::new();
            let mut latest = Vec::new();
            for e in state.msg_log.iter().rev() {
                if e.dir != Direction::Rx {
                    continue;
                }
                let k = e.msg.kind().to_string();
                if seen.contains(&k) || !pass(&k, &e.msg.type_name) {
                    continue;
                }
                seen.push(k);
                latest.push(e.clone());
                if seen.len() > 64 {
                    break;
                }
            }
            latest.sort_by(|a, b| a.msg.kind().cmp(b.msg.kind()));
            egui::ScrollArea::vertical()
                .id_salt(("latest", tab_id))
                .show(ui, |ui| {
                    for e in latest {
                        egui::CollapsingHeader::new(
                            RichText::new(format!(
                                "{}  (#{} at {:.3} s)",
                                e.msg.kind(),
                                e.n,
                                e.msg.host_ts_ns as f64 / 1e9
                            ))
                            .strong(),
                        )
                        .id_salt(("latest", tab_id, e.msg.kind().to_string()))
                        .default_open(true)
                        .show(ui, |ui| {
                            let mut path = Vec::new();
                            tree(
                                ui,
                                &e.msg.msg,
                                &mut path,
                                None,
                                egui::Id::new(("lt", tab_id)),
                            );
                        });
                    }
                });
        }
        TableMode::Log => {
            let rows: Vec<_> = state
                .msg_log
                .iter()
                .filter(|e| pass(e.msg.kind(), &e.msg.type_name))
                .cloned()
                .collect();
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 4.0;
            let mut clicked = None;
            egui::ScrollArea::both()
                .id_salt(("log", tab_id))
                .stick_to_bottom(true)
                .auto_shrink(false)
                .show_rows(ui, row_h, rows.len(), |ui, range| {
                    for e in &rows[range] {
                        let dir = match e.dir {
                            Direction::Rx => RichText::new("rx").color(OK),
                            Direction::Tx => {
                                RichText::new("tx").color(Color32::from_rgb(0x34, 0x98, 0xdb))
                            }
                        };
                        ui.horizontal(|ui| {
                            let sel = state.selected == Some(e.n);
                            if ui
                                .selectable_label(
                                    sel,
                                    RichText::new(format!(
                                        "{:>6} {:10.4}",
                                        e.n,
                                        e.msg.host_ts_ns as f64 / 1e9
                                    ))
                                    .monospace(),
                                )
                                .clicked()
                            {
                                clicked = Some(e.n);
                            }
                            ui.label(dir.monospace());
                            ui.label(
                                RichText::new(format!("{:<12}", e.msg.kind()))
                                    .monospace()
                                    .strong(),
                            );
                            let mut s = e.msg.to_json().to_string();
                            if s.len() > 200 {
                                s.truncate(200);
                                s.push('…');
                            }
                            ui.label(RichText::new(s).monospace().weak());
                        });
                    }
                });
            if let Some(n) = clicked {
                state.selected = Some(n);
                state.selected_field = None;
            }
        }
    }
}

/// Decoded tree and hex dump of the selected message.
pub fn inspector(ui: &mut egui::Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        if ui.button("Latest rx").clicked() {
            state.selected = state
                .msg_log
                .iter()
                .rev()
                .find(|e| e.dir == Direction::Rx)
                .map(|e| e.n);
            state.selected_field = None;
        }
        ui.weak("or select a row in a Table (message log).");
    });
    let Some(e) = state.selected.and_then(|n| state.logged(n)).cloned() else {
        ui.weak("No message selected.");
        return;
    };
    ui.strong(format!(
        "#{} {} {} — {} bytes",
        e.n,
        match e.dir {
            Direction::Rx => "rx",
            Direction::Tx => "tx",
        },
        e.msg.type_name,
        e.msg.raw.len()
    ));
    let off = e.msg.payload_offset;
    let desc = e.msg.msg.descriptor();
    let spans = field_spans(&e.msg.raw[off.min(e.msg.raw.len())..], Some(&desc));
    let highlight = state.selected_field.as_ref().and_then(|p| {
        let names: Vec<&str> = p.iter().map(String::as_str).collect();
        find_span(&spans, &names).map(|s| (s.start + off, s.end + off))
    });
    egui::Panel::bottom(egui::Id::new("inspector_hex"))
        .resizable(true)
        .default_size(160.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("hex")
                .show(ui, |ui| hex_dump(ui, &e.msg.raw, highlight, off));
        });
    egui::ScrollArea::vertical().id_salt("tree").show(ui, |ui| {
        let mut path = Vec::new();
        let mut sel = state.selected_field.clone();
        tree(
            ui,
            &e.msg.msg,
            &mut path,
            Some(&mut sel),
            egui::Id::new(("insp", e.n)),
        );
        state.selected_field = sel;
    });
}

fn hex_dump(ui: &mut egui::Ui, bytes: &[u8], highlight: Option<(usize, usize)>, prefix: usize) {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let text = ui.visuals().text_color();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let mut job = LayoutJob::default();
        let base = row * 16;
        job.append(
            &format!("{base:04x}  "),
            0.0,
            TextFormat::simple(font.clone(), Color32::GRAY),
        );
        for (i, b) in chunk.iter().enumerate() {
            let idx = base + i;
            let mut fmt = TextFormat::simple(
                font.clone(),
                if idx < prefix { Color32::GRAY } else { text },
            );
            if highlight.is_some_and(|(s, e)| idx >= s && idx < e) {
                fmt.background = Color32::from_rgb(0x8e, 0x44, 0xad);
                fmt.color = Color32::WHITE;
            }
            job.append(&format!("{b:02x}"), 0.0, fmt);
            job.append(" ", 0.0, TextFormat::simple(font.clone(), text));
        }
        ui.label(job);
    }
}

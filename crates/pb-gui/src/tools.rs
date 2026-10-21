//! Tool windows: schema browser, message composer, sequence runner,
//! settings and project editor.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::Receiver;
use egui::{Color32, RichText};
use prost_reflect::{
    DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, ReflectMessage, Value,
};

use pb_core::codec::{Codec, Role};
use pb_core::commands::SendRequest;
use pb_core::config::{
    ConnectionProfile, CrcKind, Endian, FramingConfig, FramingType, TransportKind,
};
use pb_core::sequence::{
    run_sequence_on, OperatorPrompt, PromptAnswer, RunEvent, RunOptions, RunReport, SequenceFile,
    StepReport, StepStatus, Target,
};

use crate::panels::{device_picker, picked, BAD, OK, WARN};
use crate::state::{AppState, OperatorReply, Settings};

// ---------------------------------------------------------------- composer

/// Message composer state.
#[derive(Default)]
pub struct ComposerState {
    /// Device to compose for (None: `main`).
    pub device: Option<String>,
    /// Selected target (oneof member or type name).
    pub target: Option<String>,
    /// The inner message being edited.
    pub msg: Option<DynamicMessage>,
    /// Show the JSON view.
    pub json_view: bool,
    json_text: String,
    json_error: Option<String>,
    /// Expected reply.
    pub expect: Option<String>,
    save_name: String,
    save_group: String,
    /// Last status line.
    pub status: Option<String>,
}

/// Codec of a device (its session's if connected).
fn device_codec(state: &AppState, device: &str) -> Option<Codec> {
    if let Some(s) = state.device_session(device) {
        return Some((**s.codec()).clone());
    }
    let d = state.project.as_ref()?.device(device)?;
    let schema = d.project.schema.as_ref()?;
    Codec::new(&d.project.config, schema, Role::Host).ok()
}

/// Names the composer can send: oneof members of tx_root (envelope), the
/// root itself, or the id-mapped types.
pub fn tx_targets(codec: &Codec) -> Vec<String> {
    match codec.mode() {
        pb_core::config::CodecMode::Envelope => {
            let Some(root) = codec.tx_root() else {
                return Vec::new();
            };
            let mut v: Vec<String> = root
                .oneofs()
                .filter(|o| !o.is_synthetic())
                .flat_map(|o| o.fields().map(|f| f.name().to_string()).collect::<Vec<_>>())
                .collect();
            v.push(root.full_name().to_string());
            v
        }
        _ => codec
            .tx_types()
            .iter()
            .map(|d| d.full_name().to_string())
            .collect(),
    }
}

/// Names the composer can expect as a reply.
pub fn rx_targets(codec: &Codec) -> Vec<String> {
    match codec.mode() {
        pb_core::config::CodecMode::Envelope => codec
            .rx_root()
            .map(|r| {
                r.oneofs()
                    .filter(|o| !o.is_synthetic())
                    .flat_map(|o| o.fields().map(|f| f.name().to_string()).collect::<Vec<_>>())
                    .collect()
            })
            .unwrap_or_default(),
        _ => codec
            .rx_types()
            .iter()
            .map(|d| d.full_name().to_string())
            .collect(),
    }
}

impl ComposerState {
    /// Selects a target and starts from an empty message.
    pub fn select(&mut self, codec: &Codec, target: &str) -> Result<(), String> {
        let t = codec.resolve_tx(target).map_err(|e| e.to_string())?;
        self.msg = Some(DynamicMessage::new(t.inner()));
        self.target = Some(target.to_string());
        self.json_error = None;
        self.refresh_json();
        Ok(())
    }

    fn refresh_json(&mut self) {
        if let Some(m) = &self.msg {
            self.json_text = serde_json::to_string_pretty(&inner_json(m)).unwrap_or_default();
        }
    }

    /// Builds the top-level request.
    pub fn request(&self, codec: &Codec, state: &AppState) -> Result<SendRequest, String> {
        let name = self.target.as_ref().ok_or("pick a message")?;
        let inner = self.msg.as_ref().ok_or("pick a message")?;
        let t = codec.resolve_tx(name).map_err(|e| e.to_string())?;
        let msg = match &t.member {
            Some(member) => {
                let mut root = DynamicMessage::new(t.root.clone());
                root.set_field(member, Value::Message(inner.clone()));
                root
            }
            None => inner.clone(),
        };
        let expect = match &self.expect {
            Some(e) => Some(codec.resolve_rx(e).map_err(|e| e.to_string())?),
            None => None,
        };
        let cfg = state
            .device_config(picked(&self.device))
            .map(|c| c.correlation.clone())
            .unwrap_or_default();
        Ok(SendRequest {
            name: format!("composer: {name}"),
            msg,
            expect,
            timeout_ms: cfg.default_timeout_ms,
            retries: cfg.default_retries,
        })
    }

    /// Sends the composed message to the selected device.
    pub fn send(&mut self, state: &mut AppState) -> Result<u64, String> {
        let device = picked(&self.device).to_string();
        let codec = state
            .device_session(&device)
            .map(|s| (**s.codec()).clone())
            .ok_or("not connected")?;
        let req = self.request(&codec, state)?;
        Ok(state.send_request_on(&device, req))
    }
}

fn inner_json(m: &DynamicMessage) -> serde_json::Value {
    let opts = prost_reflect::SerializeOptions::new()
        .use_proto_field_name(true)
        .skip_default_fields(true);
    m.serialize_with_options(serde_json::value::Serializer, &opts)
        .unwrap_or(serde_json::Value::Null)
}

/// Default value for a field kind (used when adding list items).
fn default_for(kind: &Kind) -> Value {
    match kind {
        Kind::Message(m) => Value::Message(DynamicMessage::new(m.clone())),
        Kind::Enum(e) => Value::EnumNumber(e.default_value().number()),
        Kind::Bool => Value::Bool(false),
        Kind::String => Value::String(String::new()),
        Kind::Bytes => Value::Bytes(Default::default()),
        Kind::Float => Value::F32(0.0),
        Kind::Double => Value::F64(0.0),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Value::I32(0),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Value::I64(0),
        Kind::Uint32 | Kind::Fixed32 => Value::U32(0),
        Kind::Uint64 | Kind::Fixed64 => Value::U64(0),
    }
}

fn edit_value(ui: &mut egui::Ui, v: &mut Value, kind: &Kind, id: egui::Id) -> bool {
    match (v, kind) {
        (Value::Bool(b), _) => ui.checkbox(b, "").changed(),
        (Value::I32(x), _) => ui.add(egui::DragValue::new(x)).changed(),
        (Value::I64(x), _) => ui.add(egui::DragValue::new(x)).changed(),
        (Value::U32(x), _) => ui.add(egui::DragValue::new(x)).changed(),
        (Value::U64(x), _) => ui.add(egui::DragValue::new(x)).changed(),
        (Value::F32(x), _) => ui.add(egui::DragValue::new(x).speed(0.01)).changed(),
        (Value::F64(x), _) => ui.add(egui::DragValue::new(x).speed(0.01)).changed(),
        (Value::String(s), _) => ui.text_edit_singleline(s).changed(),
        (Value::Bytes(b), _) => {
            let mut text: String =
                ui.data_mut(|d| d.get_temp(id).unwrap_or_else(|| pb_core::util::hex(b)));
            let r = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text("hex bytes")
                    .desired_width(200.0),
            );
            let mut changed = false;
            if r.changed() {
                if let Ok(bytes) = pb_core::util::parse_hex(&text) {
                    *b = bytes.into();
                    changed = true;
                }
            }
            if pb_core::util::parse_hex(&text).is_err() {
                ui.label(RichText::new("invalid hex").color(BAD));
            }
            ui.data_mut(|d| d.insert_temp(id, text));
            changed
        }
        (Value::EnumNumber(n), Kind::Enum(e)) => {
            let cur = e
                .get_value(*n)
                .map(|v| v.name().to_string())
                .unwrap_or_else(|| n.to_string());
            let mut changed = false;
            egui::ComboBox::from_id_salt(id)
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for ev in e.values() {
                        if ui.selectable_value(n, ev.number(), ev.name()).changed() {
                            changed = true;
                        }
                    }
                });
            changed
        }
        (Value::Message(m), _) => {
            let mut changed = false;
            ui.vertical(|ui| changed = edit_message(ui, m, id));
            changed
        }
        (other, _) => {
            ui.label(format!("{other:?}"));
            false
        }
    }
}

/// Form editor for a message. Returns true if anything changed.
pub fn edit_message(ui: &mut egui::Ui, msg: &mut DynamicMessage, id: egui::Id) -> bool {
    let desc = msg.descriptor();
    let mut changed = false;
    let mut done_oneofs: Vec<String> = Vec::new();
    egui::Grid::new(id.with("grid"))
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            for f in desc.fields() {
                if let Some(o) = f.containing_oneof().filter(|o| !o.is_synthetic()) {
                    if done_oneofs.contains(&o.name().to_string()) {
                        continue;
                    }
                    done_oneofs.push(o.name().to_string());
                    ui.label(RichText::new(format!("oneof {}", o.name())).italics());
                    let current = o.fields().find(|x| msg.has_field(x));
                    let mut sel = current.as_ref().map(|x| x.name().to_string());
                    let before = sel.clone();
                    ui.vertical(|ui| {
                        egui::ComboBox::from_id_salt(id.with(o.name()))
                            .selected_text(sel.clone().unwrap_or_else(|| "(none)".into()))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut sel, None, "(none)");
                                for x in o.fields() {
                                    ui.selectable_value(
                                        &mut sel,
                                        Some(x.name().to_string()),
                                        x.name(),
                                    );
                                }
                            });
                        if sel != before {
                            for x in o.fields() {
                                msg.clear_field(&x);
                            }
                            if let Some(name) = &sel {
                                if let Some(x) = desc.get_field_by_name(name) {
                                    msg.set_field(&x, default_for(&x.kind()));
                                }
                            }
                            changed = true;
                        }
                        if let Some(x) = o.fields().find(|x| msg.has_field(x)) {
                            let kind = x.kind();
                            let v = msg.get_field_mut(&x);
                            ui.horizontal(|ui| {
                                ui.label(x.name());
                                changed |= edit_value(ui, v, &kind, id.with(x.name()));
                            });
                        }
                    });
                    ui.end_row();
                    continue;
                }
                ui.label(format!("{} #{}", f.name(), f.number()));
                ui.vertical(|ui| changed |= edit_field(ui, msg, &f, id.with(f.name())));
                ui.end_row();
            }
        });
    changed
}

fn edit_field(
    ui: &mut egui::Ui,
    msg: &mut DynamicMessage,
    f: &FieldDescriptor,
    id: egui::Id,
) -> bool {
    let kind = f.kind();
    if f.is_map() {
        let v = msg.get_field(f);
        ui.label(
            RichText::new(format!(
                "{} entries (edit maps in the JSON view)",
                v.as_map().map(|m| m.len()).unwrap_or(0)
            ))
            .weak(),
        );
        return false;
    }
    if f.is_list() {
        let mut changed = false;
        let mut remove = None;
        if let Value::List(items) = msg.get_field_mut(f) {
            for (i, it) in items.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("[{i}]"));
                    changed |= edit_value(ui, it, &kind, id.with(i));
                    if ui.small_button("✖").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                items.remove(i);
                changed = true;
            }
            if ui.small_button("+ add").clicked() {
                items.push(default_for(&kind));
                changed = true;
            }
        }
        return changed;
    }
    if let Kind::Message(m) = &kind {
        let mut set = msg.has_field(f);
        let mut changed = false;
        if ui.checkbox(&mut set, m.name()).changed() {
            if set {
                msg.set_field(f, Value::Message(DynamicMessage::new(m.clone())));
            } else {
                msg.clear_field(f);
            }
            changed = true;
        }
        if set {
            egui::CollapsingHeader::new("fields")
                .id_salt(id)
                .default_open(true)
                .show(ui, |ui| {
                    if let Value::Message(inner) = msg.get_field_mut(f) {
                        changed |= edit_message(ui, inner, id.with("m"));
                    }
                });
        }
        return changed;
    }
    let v = msg.get_field_mut(f);
    edit_value(ui, v, &kind, id)
}

/// The composer window body.
pub fn composer(ui: &mut egui::Ui, cs: &mut ComposerState, state: &mut AppState) {
    let before = cs.device.clone();
    ui.horizontal(|ui| {
        if state.has_devices() {
            ui.label("Device");
            device_picker(ui, state, "composer_device", &mut cs.device, false);
        }
    });
    if cs.device != before {
        // Another device has another schema: start over.
        let device = cs.device.clone();
        *cs = ComposerState {
            device,
            ..Default::default()
        };
    }
    let device = picked(&cs.device).to_string();
    let Some(codec) = device_codec(state, &device) else {
        ui.weak("Open a project with a valid schema.");
        return;
    };
    let targets = tx_targets(&codec);
    let replies = rx_targets(&codec);
    ui.horizontal(|ui| {
        ui.label("Message");
        let mut pick = cs.target.clone();
        egui::ComboBox::from_id_salt("composer_target")
            .selected_text(pick.clone().unwrap_or_else(|| "(pick)".into()))
            .show_ui(ui, |ui| {
                for t in &targets {
                    ui.selectable_value(&mut pick, Some(t.clone()), t);
                }
            });
        if pick != cs.target {
            if let Some(t) = pick {
                if let Err(e) = cs.select(&codec, &t) {
                    cs.status = Some(e);
                }
            }
        }
        ui.label("expect");
        egui::ComboBox::from_id_salt("composer_expect")
            .selected_text(cs.expect.clone().unwrap_or_else(|| "(none)".into()))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut cs.expect, None, "(none)");
                for r in &replies {
                    ui.selectable_value(&mut cs.expect, Some(r.clone()), r);
                }
            });
    });
    if cs.msg.is_none() {
        ui.weak("Pick a message to build a form from the schema.");
        return;
    }
    ui.horizontal(|ui| {
        if ui.selectable_label(!cs.json_view, "Form").clicked() {
            cs.json_view = false;
        }
        if ui.selectable_label(cs.json_view, "JSON").clicked() {
            cs.json_view = true;
            cs.refresh_json();
        }
    });
    ui.separator();
    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 110.0)
        .show(ui, |ui| {
            if cs.json_view {
                ui.add(
                    egui::TextEdit::multiline(&mut cs.json_text)
                        .code_editor()
                        .desired_rows(10)
                        .desired_width(f32::INFINITY),
                );
                if ui.button("Apply JSON").clicked() {
                    let desc = cs.msg.as_ref().map(|m| m.descriptor());
                    if let Some(d) = desc {
                        let r = serde_json::from_str::<serde_json::Value>(&cs.json_text)
                            .map_err(|e| e.to_string())
                            .and_then(|j| {
                                pb_core::commands::normalize_json(&d, j).map_err(|e| e.to_string())
                            })
                            .and_then(|j| {
                                DynamicMessage::deserialize(d.clone(), j).map_err(|e| e.to_string())
                            });
                        match r {
                            Ok(m) => {
                                cs.msg = Some(m);
                                cs.json_error = None;
                            }
                            Err(e) => cs.json_error = Some(e),
                        }
                    }
                }
                if let Some(e) = &cs.json_error {
                    ui.label(RichText::new(e).color(BAD));
                }
            } else if let Some(m) = cs.msg.as_mut() {
                edit_message(ui, m, egui::Id::new("composer_form"));
            }
        });
    ui.separator();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                state.device_session(&device).is_some(),
                egui::Button::new("Send"),
            )
            .clicked()
        {
            cs.status = Some(match cs.send(state) {
                Ok(_) => "sent".into(),
                Err(e) => e,
            });
        }
        if let Some(t) = &cs.target {
            let key = AppState::badge_key(&device, &format!("composer: {t}"));
            if let Some(b) = state.badges.get(&key) {
                ui.label(format!("{b:?}"));
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Save as button:");
        ui.add(
            egui::TextEdit::singleline(&mut cs.save_name)
                .hint_text("name")
                .desired_width(110.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut cs.save_group)
                .hint_text("group")
                .desired_width(80.0),
        );
        let ok = !cs.save_name.trim().is_empty();
        if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
            let payload = cs.msg.as_ref().map(inner_json).unwrap_or_default();
            let group = if cs.save_group.trim().is_empty() {
                "Saved".to_string()
            } else {
                cs.save_group.trim().to_string()
            };
            let target = cs.target.clone().unwrap_or_default();
            let r = state.save_command_on(
                &device,
                cs.save_name.trim(),
                &group,
                &target,
                &payload,
                cs.expect.as_deref(),
            );
            cs.status = Some(match r {
                Ok(()) => format!("saved `{}`", cs.save_name.trim()),
                Err(e) => e,
            });
        }
    });
    if let Some(s) = &cs.status {
        ui.label(RichText::new(s).italics());
    }
}

// ---------------------------------------------------------------- schema browser

/// Schema browser state.
#[derive(Default)]
pub struct SchemaBrowserState {
    filter: String,
    show_support: bool,
    device: Option<String>,
}

fn label_of(f: &FieldDescriptor) -> String {
    let mut parts = Vec::new();
    if f.is_map() {
        parts.push("map".to_string());
    } else if f.is_list() {
        parts.push("repeated".to_string());
    } else if f.supports_presence()
        && f.containing_oneof().is_none()
        && !matches!(f.kind(), Kind::Message(_))
    {
        parts.push("optional".to_string());
    }
    if let Some(o) = f.containing_oneof().filter(|o| !o.is_synthetic()) {
        parts.push(format!("oneof {}", o.name()));
    }
    parts.join(" ")
}

fn kind_name(k: &Kind) -> String {
    match k {
        Kind::Message(m) => m.full_name().to_string(),
        Kind::Enum(e) => e.full_name().to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Schema browser body.
pub fn schema_browser(ui: &mut egui::Ui, st: &mut SchemaBrowserState, state: &AppState) {
    let schema = state
        .project
        .as_ref()
        .and_then(|p| p.device(picked(&st.device)))
        .and_then(|d| d.project.schema.clone());
    ui.horizontal(|ui| {
        device_picker(ui, state, "schema_device", &mut st.device, false);
    });
    let Some(schema) = schema else {
        ui.weak("No schema loaded.");
        return;
    };
    ui.horizontal(|ui| {
        ui.label("filter");
        ui.text_edit_singleline(&mut st.filter);
        ui.checkbox(&mut st.show_support, "show google/nanopb");
    });
    let f = st.filter.to_lowercase();
    egui::ScrollArea::vertical().show(ui, |ui| {
        let mut files: Vec<_> = schema.pool().files().collect();
        files.sort_by(|a, b| a.name().cmp(b.name()));
        for file in files {
            if !st.show_support && pb_core::schema::is_support_file(file.name()) {
                continue;
            }
            egui::CollapsingHeader::new(
                RichText::new(format!(
                    "📄 {} (package {})",
                    file.name(),
                    file.package_name()
                ))
                .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for m in file.messages() {
                    message_node(ui, &schema, &m, &f);
                }
                for e in file.enums() {
                    enum_node(ui, &schema, &e);
                }
            });
        }
    });
}

fn message_node(
    ui: &mut egui::Ui,
    schema: &pb_core::schema::Schema,
    m: &MessageDescriptor,
    filter: &str,
) {
    if !filter.is_empty()
        && !m.full_name().to_lowercase().contains(filter)
        && !m.fields().any(|f| f.name().contains(filter))
    {
        return;
    }
    egui::CollapsingHeader::new(format!("message {}", m.name()))
        .id_salt(m.full_name())
        .show(ui, |ui| {
            if let Some(c) = schema.message_comment(m) {
                ui.label(RichText::new(c).italics().weak());
            }
            egui::Grid::new(("fields", m.full_name()))
                .striped(true)
                .show(ui, |ui| {
                    for h in ["#", "name", "type", "label", "comment"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for f in m.fields() {
                        ui.monospace(f.number().to_string());
                        ui.monospace(f.name());
                        ui.monospace(kind_name(&f.kind()));
                        ui.label(label_of(&f));
                        ui.label(
                            RichText::new(schema.field_comment(&f).unwrap_or_default()).weak(),
                        );
                        ui.end_row();
                    }
                });
            for n in m.child_messages().filter(|c| !c.is_map_entry()) {
                message_node(ui, schema, &n, "");
            }
            for e in m.child_enums() {
                enum_node(ui, schema, &e);
            }
        });
}

fn enum_node(
    ui: &mut egui::Ui,
    schema: &pb_core::schema::Schema,
    e: &prost_reflect::EnumDescriptor,
) {
    egui::CollapsingHeader::new(format!("enum {}", e.name()))
        .id_salt(e.full_name())
        .show(ui, |ui| {
            if let Some(c) = schema.enum_comment(e) {
                ui.label(RichText::new(c).italics().weak());
            }
            for v in e.values() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("{} = {}", v.name(), v.number()));
                    if let Some(c) = schema.enum_value_comment(&v) {
                        ui.label(RichText::new(c).weak());
                    }
                });
            }
        });
}

// ---------------------------------------------------------------- sequence runner

enum GuiRun {
    Event(RunEvent),
    Prompt(OperatorPrompt, OperatorReply),
}

struct RunHandle {
    abort: Arc<AtomicBool>,
    events: Receiver<GuiRun>,
}

/// Sequence runner state.
#[derive(Default)]
pub struct RunnerState {
    selected: Option<String>,
    running: Option<RunHandle>,
    current: Option<(String, String)>,
    steps: Vec<StepReport>,
    log: Vec<String>,
    /// Final report of the last run.
    pub report: Option<RunReport>,
    error: Option<String>,
    /// Variables for the next run as (name, value text), from `[vars]`.
    vars: Vec<(String, String)>,
    /// Sequence `vars` was loaded for.
    vars_for: Option<String>,
}

impl RunnerState {
    /// True while a sequence runs.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Selects a sequence.
    pub fn select(&mut self, name: &str) {
        self.selected = Some(name.to_string());
    }

    /// Loads the variables table from the sequence's `[vars]` when the
    /// selection changes.
    fn sync_vars(&mut self, state: &AppState) {
        if self.vars_for == self.selected {
            return;
        }
        self.vars_for = self.selected.clone();
        self.vars = match (&self.selected, &state.project) {
            (Some(n), Some(p)) => SequenceFile::load(&p.sequence_path(n))
                .map(|s| {
                    s.vars
                        .iter()
                        .map(|(k, v)| (k.clone(), v.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
    }

    /// Starts a sequence by name, with the variables table as overrides.
    /// It runs on every device of the project that is running.
    pub fn start(&mut self, state: &AppState, name: &str) -> Result<(), String> {
        let p = state.project.as_ref().ok_or("no project")?;
        if !state.running() {
            return Err("connect first".into());
        }
        let seq = SequenceFile::load(&p.sequence_path(name)).map_err(|e| e.to_string())?;
        // Owned per-device parts for the run thread.
        let devices: Vec<(String, Option<Arc<pb_core::session::Session>>, _)> = p
            .all_devices()
            .iter()
            .map(|d| {
                (
                    d.name.to_string(),
                    state.device_session(d.name),
                    d.project.config.clone(),
                )
            })
            .collect();
        let (tx, rx) = crossbeam_channel::unbounded();
        let abort = Arc::new(AtomicBool::new(false));
        let vars = if self.vars_for.as_deref() == Some(name) {
            self.vars
                .iter()
                .filter(|(k, _)| !k.trim().is_empty())
                .map(|(k, v)| {
                    (
                        k.trim().to_string(),
                        pb_core::commands::parse_value(v.trim()),
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let opts = RunOptions {
            non_interactive: None,
            abort: abort.clone(),
            vars,
        };
        let ptx = tx.clone();
        std::thread::Builder::new()
            .name("protobench-seq".into())
            .spawn(move || {
                let mut on_event = |ev: RunEvent| {
                    let _ = tx.send(GuiRun::Event(ev));
                };
                let prompt = Box::new(move |p: &OperatorPrompt| {
                    let (rtx, rrx) = crossbeam_channel::bounded(1);
                    if ptx.send(GuiRun::Prompt(p.clone(), rtx)).is_err() {
                        return PromptAnswer::Fail;
                    }
                    match rrx.recv() {
                        Ok(Some(values)) => PromptAnswer::Continue(values),
                        _ => PromptAnswer::Fail,
                    }
                });
                let targets: Vec<Target> = devices
                    .iter()
                    .map(|(name, session, config)| Target {
                        name,
                        session: session.as_deref(),
                        config,
                    })
                    .collect();
                run_sequence_on(&targets, &seq, opts, &mut on_event, Some(prompt));
            })
            .map_err(|e| e.to_string())?;
        self.running = Some(RunHandle { abort, events: rx });
        self.steps.clear();
        self.log.clear();
        self.report = None;
        self.current = None;
        self.error = None;
        Ok(())
    }

    /// Requests an abort.
    pub fn abort(&self) {
        if let Some(r) = &self.running {
            r.abort.store(true, Ordering::Relaxed);
        }
    }
}

/// Drains runner events (called every frame, window open or not).
pub fn runner_poll(rs: &mut RunnerState, state: &mut AppState) {
    let Some(h) = &rs.running else { return };
    let mut finished = false;
    while let Ok(ev) = h.events.try_recv() {
        match ev {
            GuiRun::Event(RunEvent::StepStarted { index, label }) => {
                rs.current = Some((index, label))
            }
            GuiRun::Event(RunEvent::StepFinished(r)) => {
                rs.current = None;
                rs.steps.push(r);
            }
            GuiRun::Event(RunEvent::Log(l)) => rs.log.push(l),
            GuiRun::Event(RunEvent::Finished(r)) => {
                state.notify(format!(
                    "sequence `{}` {}",
                    r.sequence,
                    if r.passed { "passed" } else { "failed" }
                ));
                rs.report = Some(*r);
                finished = true;
            }
            GuiRun::Prompt(prompt, reply) => state.open_operator(prompt, reply),
        }
    }
    if finished {
        rs.running = None;
    }
}

fn status_icon(s: StepStatus) -> RichText {
    match s {
        StepStatus::Pass => RichText::new("✔ pass").color(OK),
        StepStatus::Fail => RichText::new("✖ fail").color(BAD),
        StepStatus::Error => RichText::new("✖ error").color(BAD),
        StepStatus::Skipped => RichText::new("– skipped").color(Color32::GRAY),
    }
}

/// Sequence runner body.
pub fn runner(ui: &mut egui::Ui, rs: &mut RunnerState, state: &mut AppState) {
    let names = state
        .project
        .as_ref()
        .map(|p| p.sequence_names())
        .unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label("Sequence");
        egui::ComboBox::from_id_salt("seq_pick")
            .selected_text(rs.selected.clone().unwrap_or_else(|| "(pick)".into()))
            .show_ui(ui, |ui| {
                for n in &names {
                    ui.selectable_value(&mut rs.selected, Some(n.clone()), n);
                }
            });
        if rs.is_running() {
            if ui.button("■ Abort").clicked() {
                rs.abort();
            }
            ui.spinner();
        } else {
            let can = rs.selected.is_some() && state.running();
            if ui.add_enabled(can, egui::Button::new("▶ Run")).clicked() {
                if let Some(n) = rs.selected.clone() {
                    if let Err(e) = rs.start(state, &n) {
                        rs.error = Some(e);
                    }
                }
            }
        }
        if let Some(r) = &rs.report {
            if ui.button("Save report…").clicked() {
                let start = state.project.as_ref().map(|p| p.dir.join("reports"));
                let mut d = rfd::FileDialog::new().set_title("Report folder");
                if let Some(s) = start {
                    let _ = std::fs::create_dir_all(&s);
                    d = d.set_directory(s);
                }
                if let Some(dir) = d.pick_folder() {
                    let stem = rs.selected.clone().unwrap_or_else(|| "sequence".into());
                    match r.write(&dir, &stem) {
                        Ok((j, x)) => {
                            state.notify(format!("saved {} and {}", j.display(), x.display()))
                        }
                        Err(e) => state.notify(format!("cannot save report: {e}")),
                    }
                }
            }
        }
    });
    rs.sync_vars(state);
    if rs.selected.is_some() {
        egui::CollapsingHeader::new(format!("Variables ({})", rs.vars.len()))
            .id_salt("seq_vars")
            .show(ui, |ui| {
                ui.add_enabled_ui(!rs.is_running(), |ui| {
                    let mut remove = None;
                    egui::Grid::new("seq_vars_grid").show(ui, |ui| {
                        for (i, (k, v)) in rs.vars.iter_mut().enumerate() {
                            ui.add(egui::TextEdit::singleline(k).desired_width(120.0));
                            ui.add(egui::TextEdit::singleline(v).desired_width(160.0));
                            if ui.small_button("✖").on_hover_text("Remove").clicked() {
                                remove = Some(i);
                            }
                            ui.end_row();
                        }
                    });
                    if let Some(i) = remove {
                        rs.vars.remove(i);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("+ Variable").clicked() {
                            rs.vars.push((String::new(), String::new()));
                        }
                        if ui
                            .button("Reset")
                            .on_hover_text("Reload from [vars]")
                            .clicked()
                        {
                            rs.vars_for = None;
                        }
                    });
                    ui.weak("Values in TOML syntax; they override [vars] for the next run.");
                });
            });
    }
    if !state.running() {
        ui.weak("Connect to run sequences.");
    }
    if let Some(e) = &rs.error {
        ui.label(RichText::new(e).color(BAD));
    }
    if let Some(r) = &rs.report {
        let s = r.summary;
        let text = format!(
            "{} — {} passed, {} failed, {} errors, {} skipped in {:.2} s",
            if r.passed { "PASSED" } else { "FAILED" },
            s.passed,
            s.failed,
            s.errors,
            s.skipped,
            r.duration_s
        );
        ui.label(
            RichText::new(text)
                .strong()
                .color(if r.passed { OK } else { BAD }),
        );
    }
    ui.separator();
    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 120.0)
        .show(ui, |ui| {
            for st in &rs.steps {
                ui.horizontal(|ui| {
                    ui.label(status_icon(st.status));
                    ui.monospace(format!("{:>7}", st.index));
                    ui.label(&st.label);
                    ui.weak(format!("{:.0} ms", st.duration_ms));
                    if let Some(m) = &st.message {
                        ui.label(RichText::new(m).color(if st.status == StepStatus::Pass {
                            Color32::GRAY
                        } else {
                            WARN
                        }));
                    }
                });
                for a in &st.assertions {
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        ui.label(
                            RichText::new(if a.passed { "✔" } else { "✖" }).color(if a.passed {
                                OK
                            } else {
                                BAD
                            }),
                        );
                        ui.label(format!("{} = {}", a.description, a.measured));
                    });
                }
            }
            if let Some((i, l)) = &rs.current {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.monospace(format!("{i:>7}"));
                    ui.label(l);
                });
            }
        });
    ui.separator();
    ui.label("Log");
    egui::ScrollArea::vertical()
        .id_salt("seq_log")
        .max_height(100.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for l in &rs.log {
                ui.monospace(l);
            }
        });
}

// ---------------------------------------------------------------- settings

/// Settings body.
pub fn settings(ui: &mut egui::Ui, s: &mut Settings) {
    egui::Grid::new("settings").show(ui, |ui| {
        ui.label("Theme");
        ui.horizontal(|ui| {
            ui.radio_value(&mut s.dark, true, "dark");
            ui.radio_value(&mut s.dark, false, "light");
        });
        ui.end_row();
        ui.label("Ring buffer per channel");
        ui.add(
            egui::DragValue::new(&mut s.capacity)
                .range(1000..=50_000_000)
                .speed(1000)
                .suffix(" points"),
        );
        ui.end_row();
        ui.label("Plot frame-rate cap");
        ui.add(egui::Slider::new(&mut s.fps_cap, 5..=144).suffix(" fps"));
        ui.end_row();
        ui.label("Recordings folder");
        ui.horizontal(|ui| {
            let mut text = s
                .recordings_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            if ui
                .add(
                    egui::TextEdit::singleline(&mut text)
                        .hint_text("<project>/recordings")
                        .desired_width(220.0),
                )
                .changed()
            {
                s.recordings_dir = (!text.trim().is_empty()).then(|| PathBuf::from(text.trim()));
            }
            if ui.button("Browse…").clicked() {
                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                    s.recordings_dir = Some(d);
                }
            }
        });
        ui.end_row();
        ui.label("Foxglove server");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut s.foxglove_bind)
                    .desired_width(110.0)
                    .hint_text("127.0.0.1"),
            )
            .on_hover_text("0.0.0.0 serves other machines on the network");
            ui.label(":");
            ui.add(egui::DragValue::new(&mut s.foxglove_port).range(0..=65535));
        });
        ui.end_row();
    });
    ui.weak("The ring-buffer size applies on the next connect, the Foxglove address when the server next starts.");
}

// ---------------------------------------------------------------- project editor

/// Project editor state (a working copy of profiles and framing).
#[derive(Default)]
pub struct ProjectEditorState {
    profiles: Option<Vec<ConnectionProfile>>,
    framing: Option<FramingConfig>,
    sync_text: String,
    status: Option<String>,
}

fn transport_label(t: TransportKind) -> &'static str {
    t.as_str()
}

/// A warning when the framing does not suit the link (the same rule as
/// `protobench check`).
fn framing_hint(t: TransportKind, framing: FramingType) -> Option<&'static str> {
    use pb_core::config::LinkKind;
    match (t.link_kind(), framing == FramingType::None) {
        (LinkKind::Stream, true) => {
            Some("byte stream: framing \"none\" cannot find frame boundaries")
        }
        (LinkKind::Message, false) => {
            Some("delivers whole messages: framing \"none\" is the usual choice")
        }
        _ => None,
    }
}

/// The attached serial ports: pick one by name, or match a USB board by
/// VID/PID and serial number (`port = "auto"`).
fn port_menu(ui: &mut egui::Ui, pr: &mut ConnectionProfile) {
    if ui.button("auto").clicked() {
        pr.port = "auto".into();
        ui.close();
    }
    let ports = pb_core::transport::list_ports();
    if ports.is_empty() {
        ui.weak("no serial ports found");
    }
    for port in ports {
        let mut label = format!("{}  {}", port.name, port.usb_id());
        if let Some(s) = &port.serial_number {
            label.push_str(&format!("  #{s}"));
        }
        if let Some(d) = &port.description {
            label.push_str(&format!("  {d}"));
        }
        ui.menu_button(label, |ui| {
            if ui.button(format!("Open {}", port.name)).clicked() {
                pr.port = port.name.clone();
                ui.close();
            }
            if port.vid.is_some()
                && ui
                    .button("Match this board (auto by USB ID and serial number)")
                    .clicked()
            {
                pr.port = "auto".into();
                pr.usb_vid = port.vid;
                pr.usb_pid = port.pid;
                pr.usb_serial = port.serial_number.clone();
                ui.close();
            }
        });
    }
}

/// A hex text field for an optional USB ID; empty means "any".
fn hex_u16_edit(ui: &mut egui::Ui, v: &mut Option<u16>) {
    let mut text = v.map(|x| format!("{x:04x}")).unwrap_or_default();
    let r = ui.add(
        egui::TextEdit::singleline(&mut text)
            .desired_width(48.0)
            .hint_text("any"),
    );
    if r.changed() {
        let t = text.trim().trim_start_matches("0x");
        if t.is_empty() {
            *v = None;
        } else if let Ok(x) = u16::from_str_radix(t, 16) {
            *v = Some(x);
        }
    }
}

/// Project editor body. Returns true if the project was saved and reloaded.
pub fn project_editor(
    ui: &mut egui::Ui,
    es: &mut ProjectEditorState,
    state: &mut AppState,
) -> bool {
    let Some(p) = &state.project else {
        ui.weak("No project.");
        return false;
    };
    if !p.config_ok {
        ui.label(
            RichText::new("project.toml has errors; fix it in a text editor first.").color(BAD),
        );
        return false;
    }
    let config_path = p.config_path();
    if es.profiles.is_none() {
        es.profiles = Some(p.config.connection.profiles.clone());
        es.framing = Some(p.config.framing.clone());
        es.sync_text = pb_core::util::hex(&p.config.framing.sync_word);
    }
    let mut saved = false;
    let framing_kind = es.framing.as_ref().map(|f| f.kind).unwrap_or_default();
    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 40.0)
        .show(ui, |ui| {
            ui.heading("Connection profiles");
            let profiles = es.profiles.get_or_insert_with(Vec::new);
            let mut remove = None;
            for (i, pr) in profiles.iter_mut().enumerate() {
                egui::CollapsingHeader::new(format!("{} ({})", pr.name, pr.transport.as_str()))
                    .id_salt(("prof", i))
                    .show(ui, |ui| {
                        egui::Grid::new(("profgrid", i)).show(ui, |ui| {
                            ui.label("name");
                            ui.text_edit_singleline(&mut pr.name);
                            ui.end_row();
                            ui.label("transport");
                            egui::ComboBox::from_id_salt(("tr", i))
                                .selected_text(transport_label(pr.transport))
                                .show_ui(ui, |ui| {
                                    for &t in TransportKind::ALL {
                                        ui.selectable_value(&mut pr.transport, t, t.as_str());
                                    }
                                });
                            ui.end_row();
                            if let Some(w) = framing_hint(pr.transport, framing_kind) {
                                ui.label("");
                                ui.label(RichText::new(w).color(WARN));
                                ui.end_row();
                            }
                            match pr.transport {
                                TransportKind::Serial => {
                                    ui.label("port");
                                    ui.horizontal(|ui| {
                                        ui.text_edit_singleline(&mut pr.port);
                                        ui.menu_button("▾", |ui| port_menu(ui, pr));
                                    });
                                    ui.end_row();
                                    if pr.port == "auto" {
                                        ui.label("USB VID / PID (hex)");
                                        ui.horizontal(|ui| {
                                            hex_u16_edit(ui, &mut pr.usb_vid);
                                            hex_u16_edit(ui, &mut pr.usb_pid);
                                        });
                                        ui.end_row();
                                        ui.label("USB serial number");
                                        let mut serial = pr.usb_serial.clone().unwrap_or_default();
                                        if ui
                                            .add(
                                                egui::TextEdit::singleline(&mut serial)
                                                    .hint_text("any"),
                                            )
                                            .changed()
                                        {
                                            pr.usb_serial = (!serial.trim().is_empty())
                                                .then(|| serial.trim().to_string());
                                        }
                                        ui.end_row();
                                    }
                                    ui.label("baud");
                                    ui.add(
                                        egui::DragValue::new(&mut pr.baud).range(50..=20_000_000),
                                    );
                                    ui.end_row();
                                    ui.label("data bits / stop bits");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::DragValue::new(&mut pr.data_bits).range(5..=8),
                                        );
                                        ui.add(
                                            egui::DragValue::new(&mut pr.stop_bits).range(1..=2),
                                        );
                                    });
                                    ui.end_row();
                                    ui.label("parity");
                                    egui::ComboBox::from_id_salt(("par", i))
                                        .selected_text(format!("{:?}", pr.parity))
                                        .show_ui(ui, |ui| {
                                            for x in [
                                                pb_core::config::Parity::None,
                                                pb_core::config::Parity::Odd,
                                                pb_core::config::Parity::Even,
                                            ] {
                                                ui.selectable_value(
                                                    &mut pr.parity,
                                                    x,
                                                    format!("{x:?}"),
                                                );
                                            }
                                        });
                                    ui.end_row();
                                    ui.label("flow control");
                                    egui::ComboBox::from_id_salt(("flow", i))
                                        .selected_text(format!("{:?}", pr.flow_control))
                                        .show_ui(ui, |ui| {
                                            for x in [
                                                pb_core::config::FlowControl::None,
                                                pb_core::config::FlowControl::Software,
                                                pb_core::config::FlowControl::Hardware,
                                            ] {
                                                ui.selectable_value(
                                                    &mut pr.flow_control,
                                                    x,
                                                    format!("{x:?}"),
                                                );
                                            }
                                        });
                                    ui.end_row();
                                    ui.label("DTR / RTS on open");
                                    ui.horizontal(|ui| {
                                        ui.checkbox(&mut pr.dtr_on_open, "DTR");
                                        ui.checkbox(&mut pr.rts_on_open, "RTS");
                                    });
                                    ui.end_row();
                                }
                                TransportKind::TcpClient => {
                                    ui.label("host");
                                    ui.text_edit_singleline(&mut pr.host);
                                    ui.end_row();
                                    ui.label("tcp_port");
                                    ui.add(egui::DragValue::new(&mut pr.tcp_port));
                                    ui.end_row();
                                }
                                TransportKind::TcpServer => {
                                    ui.label("tcp_port");
                                    ui.add(egui::DragValue::new(&mut pr.tcp_port));
                                    ui.end_row();
                                }
                                TransportKind::Udp => {
                                    let mut bind = pr.bind.clone().unwrap_or_default();
                                    ui.label("bind");
                                    if ui.text_edit_singleline(&mut bind).changed() {
                                        pr.bind = (!bind.is_empty()).then_some(bind);
                                    }
                                    ui.end_row();
                                    let mut target = pr.target.clone().unwrap_or_default();
                                    ui.label("target");
                                    if ui.text_edit_singleline(&mut target).changed() {
                                        pr.target = (!target.is_empty()).then_some(target);
                                    }
                                    ui.end_row();
                                }
                                TransportKind::Replay => {
                                    let mut file = pr.file.clone().unwrap_or_default();
                                    ui.label("file");
                                    if ui.text_edit_singleline(&mut file).changed() {
                                        pr.file = (!file.is_empty()).then_some(file);
                                    }
                                    ui.end_row();
                                    ui.label("speed (0 = max)");
                                    ui.add(
                                        egui::DragValue::new(&mut pr.speed)
                                            .range(0.0..=1000.0)
                                            .speed(0.1),
                                    );
                                    ui.end_row();
                                }
                                TransportKind::Sim => {}
                            }
                            ui.label("auto reconnect");
                            ui.checkbox(&mut pr.auto_reconnect, "");
                            ui.end_row();
                        });
                        if ui.button("Remove profile").clicked() {
                            remove = Some(i);
                        }
                    });
            }
            if let Some(i) = remove {
                profiles.remove(i);
            }
            if ui.button("+ Add profile").clicked() {
                profiles.push(ConnectionProfile {
                    name: format!("profile{}", profiles.len() + 1),
                    ..Default::default()
                });
            }
            ui.separator();
            ui.heading("Framing");
            let fr = es.framing.get_or_insert_with(FramingConfig::default);
            egui::Grid::new("framing").show(ui, |ui| {
                ui.label("type");
                egui::ComboBox::from_id_salt("ftype")
                    .selected_text(format!("{:?}", fr.kind))
                    .show_ui(ui, |ui| {
                        for x in [
                            FramingType::Cobs,
                            FramingType::Slip,
                            FramingType::VarintLen,
                            FramingType::FixedLen,
                            FramingType::None,
                        ] {
                            ui.selectable_value(&mut fr.kind, x, format!("{x:?}"));
                        }
                    });
                ui.end_row();
                ui.label("crc");
                egui::ComboBox::from_id_salt("fcrc")
                    .selected_text(format!("{:?}", fr.crc))
                    .show_ui(ui, |ui| {
                        for x in [
                            CrcKind::None,
                            CrcKind::Crc8,
                            CrcKind::Crc16CcittFalse,
                            CrcKind::Crc16Modbus,
                            CrcKind::Crc32,
                        ] {
                            ui.selectable_value(&mut fr.crc, x, format!("{x:?}"));
                        }
                    });
                ui.end_row();
                ui.label("crc endian");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut fr.crc_endian, Endian::Little, "little");
                    ui.radio_value(&mut fr.crc_endian, Endian::Big, "big");
                });
                ui.end_row();
                if fr.kind == FramingType::FixedLen {
                    ui.label("len_bytes / endian");
                    ui.horizontal(|ui| {
                        for n in [1u8, 2, 4] {
                            ui.radio_value(&mut fr.len_bytes, n, n.to_string());
                        }
                        ui.radio_value(&mut fr.endian, Endian::Little, "LE");
                        ui.radio_value(&mut fr.endian, Endian::Big, "BE");
                    });
                    ui.end_row();
                }
                if matches!(fr.kind, FramingType::FixedLen | FramingType::VarintLen) {
                    ui.label("sync word (hex)");
                    ui.text_edit_singleline(&mut es.sync_text);
                    ui.end_row();
                }
                ui.label("max_frame_len");
                ui.add(egui::DragValue::new(&mut fr.max_frame_len).range(1..=16_777_216));
                ui.end_row();
                ui.label("text passthrough");
                ui.checkbox(&mut fr.text_passthrough, "");
                ui.end_row();
            });
        });
    ui.horizontal(|ui| {
        if ui.button("Save to project.toml").clicked() {
            let mut fr = es.framing.clone().unwrap_or_default();
            match pb_core::util::parse_hex(&es.sync_text) {
                Ok(s) => fr.sync_word = s,
                Err(e) => {
                    es.status = Some(format!("sync word: {e}"));
                    return;
                }
            }
            let profiles = es.profiles.clone().unwrap_or_default();
            match write_project_settings(&config_path, &profiles, &fr) {
                Ok(()) => {
                    state.reload_project();
                    es.profiles = None;
                    es.status = Some("saved and reloaded".into());
                    saved = true;
                }
                Err(e) => es.status = Some(e),
            }
        }
        if ui.button("Revert").clicked() {
            es.profiles = None;
            es.status = None;
        }
        if let Some(s) = &es.status {
            ui.label(s);
        }
    });
    saved
}

/// Writes profiles and framing into project.toml with `toml_edit`. Framing
/// keys are updated in place (comments kept); the profile array is replaced.
pub fn write_project_settings(
    path: &std::path::Path,
    profiles: &[ConnectionProfile],
    framing: &FramingConfig,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let out = edit_project_settings(&text, profiles, framing)?;
    std::fs::write(path, out).map_err(|e| e.to_string())
}

/// The text transformation behind [`write_project_settings`].
pub fn edit_project_settings(
    text: &str,
    profiles: &[ConnectionProfile],
    framing: &FramingConfig,
) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("project.toml: {e}"))?;
    // Framing: update keys in place.
    let fr_toml = toml::Value::try_from(framing).map_err(|e| e.to_string())?;
    let fr_doc: toml_edit::DocumentMut = format!(
        "[framing]\n{}",
        toml::to_string(&fr_toml).map_err(|e| e.to_string())?
    )
    .parse()
    .map_err(|e| format!("{e}"))?;
    if doc.get("framing").is_none() {
        doc.insert("framing", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    if let (Some(dst), Some(src)) = (doc["framing"].as_table_mut(), fr_doc["framing"].as_table()) {
        for (k, v) in src.iter() {
            if let Some(val) = v.as_value() {
                match dst.get_mut(k).and_then(|i| i.as_value_mut()) {
                    Some(existing) => {
                        let decor = existing.decor().clone();
                        *existing = val.clone();
                        *existing.decor_mut() = decor;
                    }
                    None => {
                        dst.insert(k, toml_edit::Item::Value(val.clone()));
                    }
                }
            }
        }
        if framing.sync_word.is_empty() {
            dst.remove("sync_word");
        }
    }
    // Profiles: non-default keys only.
    let default = toml::Value::try_from(ConnectionProfile::default()).map_err(|e| e.to_string())?;
    let mut s = String::new();
    for p in profiles {
        let v = toml::Value::try_from(p).map_err(|e| e.to_string())?;
        let mut t = v.as_table().cloned().unwrap_or_default();
        if let Some(d) = default.as_table() {
            t.retain(|k, v| k == "name" || k == "transport" || d.get(k) != Some(v));
        }
        s.push_str("[[profiles]]\n");
        s.push_str(&toml::to_string(&t).map_err(|e| e.to_string())?);
        s.push('\n');
    }
    let pdoc: toml_edit::DocumentMut = s.parse().map_err(|e| format!("{e}"))?;
    if doc.get("connection").is_none() {
        doc.insert(
            "connection",
            toml_edit::Item::Table(toml_edit::Table::new()),
        );
    }
    let item = pdoc
        .get("profiles")
        .cloned()
        .unwrap_or(toml_edit::Item::ArrayOfTables(Default::default()));
    if let Some(conn) = doc["connection"].as_table_mut() {
        conn.insert("profiles", item);
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn demo_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor")
    }

    #[test]
    fn composer_builds_and_sends_every_host_to_device_command() {
        let mut state = AppState::default();
        state.open_project(&demo_dir());
        state.connect("sim").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.connected() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let codec = (**state.session.as_ref().unwrap().codec()).clone();
        let targets = tx_targets(&codec);
        for want in [
            "ping",
            "set_bias",
            "stream_ctrl",
            "get_status",
            "erase_cal",
            "demo.HostToDevice",
        ] {
            assert!(targets.contains(&want.to_string()), "{targets:?}");
        }
        let mut sent = Vec::new();
        for t in targets.iter().filter(|t| !t.contains('.')) {
            let mut cs = ComposerState::default();
            cs.select(&codec, t).unwrap();
            // Fill a field through the same Value API the form uses.
            if let Some(m) = cs.msg.as_mut() {
                if let Some(f) = m.descriptor().fields().next() {
                    let v = default_for(&f.kind());
                    m.set_field(&f, v);
                }
            }
            if t == "set_bias" {
                cs.expect = Some("ack".into());
            }
            let req = cs.request(&codec, &state).unwrap();
            assert_eq!(req.msg.descriptor().full_name(), "demo.HostToDevice");
            assert_eq!(
                pb_core::codec::set_oneof_member(&req.msg).as_deref(),
                Some(t.as_str())
            );
            sent.push((t.clone(), cs.send(&mut state).unwrap()));
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            state.poll();
            let done = sent.iter().all(|(t, _)| {
                matches!(
                    state.badges.get(&format!("composer: {t}")),
                    Some(crate::state::Badge::Sent) | Some(crate::state::Badge::Ok { .. })
                )
            });
            if done {
                break;
            }
            assert!(Instant::now() < deadline, "{:?}", state.badges);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state.session.as_ref().unwrap().stats().errors(), 0);
    }

    #[test]
    fn json_round_trip_of_composed_message() {
        let p = pb_core::project::Project::open(&demo_dir());
        let codec = Codec::new(&p.config, p.schema.as_ref().unwrap(), Role::Host).unwrap();
        let mut cs = ComposerState::default();
        cs.select(&codec, "stream_ctrl").unwrap();
        let d = cs.msg.as_ref().unwrap().descriptor();
        let j = serde_json::json!({"enable": true, "rate_hz": 250});
        let m = DynamicMessage::deserialize(
            d,
            pb_core::commands::normalize_json(&cs.msg.as_ref().unwrap().descriptor(), j).unwrap(),
        )
        .unwrap();
        assert_eq!(
            inner_json(&m),
            serde_json::json!({"enable": true, "rate_hz": 250})
        );
    }

    #[test]
    fn project_settings_edit_keeps_comments() {
        let text = std::fs::read_to_string(demo_dir().join("project.toml")).unwrap();
        let cfg = pb_core::config::load_config_str(&text, Path::new("p.toml"))
            .config
            .unwrap();
        let mut fr = cfg.framing.clone();
        fr.max_frame_len = 2048;
        let mut profiles = cfg.connection.profiles.clone();
        profiles[0].baud = 115_200;
        let out = edit_project_settings(&text, &profiles, &fr).unwrap();
        assert!(out.contains("max_frame_len = 2048"));
        assert!(
            out.contains("# cobs | slip | varint_len | fixed_len | none"),
            "comments kept"
        );
        let back = pb_core::config::load_config_str(&out, Path::new("p.toml"));
        let c = back.config.unwrap();
        assert!(
            back.diagnostics.iter().all(|d| !d.is_error()),
            "{:?}",
            back.diagnostics
        );
        assert_eq!(c.framing.max_frame_len, 2048);
        assert_eq!(c.connection.profiles[0].baud, 115_200);
        assert_eq!(c.connection.profiles.len(), 3);
        assert_eq!(c.connection.profiles[0].usb_vid, Some(0x0483));
        assert_eq!(c.commands.len(), cfg.commands.len());
    }
}

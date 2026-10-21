//! Dockable panel model and `layout.json` persistence.

use std::path::Path;

use egui_dock::{DockState, NodeIndex};
use serde::{Deserialize, Serialize};

use pb_core::project::Project;

/// Current `layout.json` format version.
pub const LAYOUT_VERSION: u32 = 1;

/// Per-user working layout next to `layout.json`; saved automatically and
/// meant to be git-ignored, so using the GUI never changes the project.
pub const LOCAL_LAYOUT_FILE: &str = "layout.local.json";

/// Palette for new traces.
pub const PALETTE: [[u8; 3]; 10] = [
    [0x1f, 0x77, 0xb4],
    [0xff, 0x7f, 0x0e],
    [0x2c, 0xa0, 0x2c],
    [0xd6, 0x27, 0x28],
    [0x94, 0x67, 0xbd],
    [0x8c, 0x56, 0x4b],
    [0xe3, 0x77, 0xc2],
    [0x7f, 0x7f, 0x7f],
    [0xbc, 0xbd, 0x22],
    [0x17, 0xbe, 0xcf],
];

/// One trace on a time plot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    /// Channel name.
    pub channel: String,
    /// RGB colour.
    pub color: [u8; 3],
    /// Y axis: 1 (left) or 2 (right).
    #[serde(default = "one")]
    pub axis: u8,
    /// Shown or hidden.
    #[serde(default = "yes")]
    pub visible: bool,
}

fn one() -> u8 {
    1
}
fn yes() -> bool {
    true
}
fn ten() -> f64 {
    10.0
}

/// Runtime view state of a time plot (not saved).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlotRuntime {
    /// Frozen right edge when paused.
    pub frozen_at: Option<f64>,
    /// User zoomed or panned; the view stops following.
    pub manual: bool,
    /// Cursor A time.
    pub cursor_a: Option<f64>,
    /// Cursor B time.
    pub cursor_b: Option<f64>,
}

/// Time plot settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimePlotCfg {
    /// Traces.
    #[serde(default)]
    pub traces: Vec<Trace>,
    /// Visible window in seconds.
    #[serde(default = "ten")]
    pub window_s: f64,
    /// Fit Y to the data.
    #[serde(default = "yes")]
    pub autoscale: bool,
    /// Show the channel picker beside the plot.
    #[serde(default)]
    pub show_picker: bool,
    /// View state.
    #[serde(skip)]
    pub rt: PlotRuntime,
}

impl Default for TimePlotCfg {
    fn default() -> Self {
        Self {
            traces: Vec::new(),
            window_s: 10.0,
            autoscale: true,
            show_picker: true,
            rt: PlotRuntime::default(),
        }
    }
}

impl TimePlotCfg {
    /// Adds a channel with the next palette colour (no duplicates).
    pub fn add_channel(&mut self, name: &str) {
        if self.traces.iter().any(|t| t.channel == name) {
            return;
        }
        let color = PALETTE[self.traces.len() % PALETTE.len()];
        self.traces.push(Trace {
            channel: name.to_string(),
            color,
            axis: 1,
            visible: true,
        });
    }
}

/// XY plot settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct XyPlotCfg {
    /// X channel.
    pub x: Option<String>,
    /// Y channel.
    pub y: Option<String>,
    /// Points to show (newest).
    #[serde(default)]
    pub points: usize,
}

/// Channel list settings for readouts and indicators (empty = all).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelsCfg {
    /// Channels shown; empty means all suitable channels.
    #[serde(default)]
    pub channels: Vec<String>,
}

/// Table modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableMode {
    /// Latest value of every field.
    #[default]
    Latest,
    /// Scrolling log of decoded messages.
    Log,
}

/// Table settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TableCfg {
    /// Mode.
    #[serde(default)]
    pub mode: TableMode,
    /// Message-type filter (substring).
    #[serde(default)]
    pub filter: String,
}

/// Raw console settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawCfg {
    /// Show only this device (None: all, tagged when there are several).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Show hex.
    #[serde(default = "yes")]
    pub hex: bool,
    /// Show ASCII.
    #[serde(default = "yes")]
    pub ascii: bool,
    /// Search text (hex or ASCII).
    #[serde(default)]
    pub search: String,
    /// Send box: text or hex.
    #[serde(default)]
    pub send_hex: bool,
    /// Paused (not saved).
    #[serde(skip)]
    pub paused_at: Option<usize>,
    /// Send box contents (not saved).
    #[serde(skip)]
    pub input: String,
}

impl Default for RawCfg {
    fn default() -> Self {
        Self {
            device: None,
            hex: true,
            ascii: true,
            search: String::new(),
            send_hex: true,
            paused_at: None,
            input: String::new(),
        }
    }
}

/// Text console settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextCfg {
    /// Show only this device (None: all, tagged when there are several).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Filter (substring, case-insensitive).
    #[serde(default)]
    pub filter: String,
    /// Follow the newest line.
    #[serde(default = "yes")]
    pub autoscroll: bool,
}

impl Default for TextCfg {
    fn default() -> Self {
        Self {
            device: None,
            filter: String::new(),
            autoscroll: true,
        }
    }
}

/// Histogram / FFT settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistCfg {
    /// Channel.
    pub channel: Option<String>,
    /// Number of bins (histogram).
    #[serde(default = "bins")]
    pub bins: usize,
    /// Show the FFT magnitude instead of the histogram.
    #[serde(default)]
    pub fft: bool,
    /// Samples analysed (newest).
    #[serde(default = "fft_n")]
    pub samples: usize,
}

fn bins() -> usize {
    50
}
fn fft_n() -> usize {
    4096
}

impl Default for HistCfg {
    fn default() -> Self {
        Self {
            channel: None,
            bins: 50,
            fft: false,
            samples: 4096,
        }
    }
}

/// Every panel type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Panel {
    /// Time plot.
    TimePlot(TimePlotCfg),
    /// XY plot.
    XyPlot(XyPlotCfg),
    /// Numeric readout tiles.
    Readouts(ChannelsCfg),
    /// Enum/bool indicators.
    Indicators(ChannelsCfg),
    /// Latest values or message log.
    Table(TableCfg),
    /// Decoded tree + hex of the selected message.
    Inspector,
    /// Raw bytes.
    RawConsole(RawCfg),
    /// Firmware text lines.
    TextConsole(TextCfg),
    /// Per-channel statistics.
    ChannelStats,
    /// Link counters, bad frames, latency.
    LinkStats,
    /// Histogram / FFT.
    Histogram(HistCfg),
    /// Command buttons.
    CommandBar,
    /// Draggable list of channels.
    Channels,
    /// Sent-command history.
    History,
    /// Markers and notes.
    Markers,
}

impl Panel {
    /// Default title.
    pub fn title(&self) -> &'static str {
        match self {
            Panel::TimePlot(_) => "Time Plot",
            Panel::XyPlot(_) => "XY Plot",
            Panel::Readouts(_) => "Readouts",
            Panel::Indicators(_) => "Indicators",
            Panel::Table(_) => "Table",
            Panel::Inspector => "Message Inspector",
            Panel::RawConsole(_) => "Raw Console",
            Panel::TextConsole(_) => "Text Console",
            Panel::ChannelStats => "Channel Statistics",
            Panel::LinkStats => "Link Statistics",
            Panel::Histogram(_) => "Histogram / FFT",
            Panel::CommandBar => "Commands",
            Panel::Channels => "Channels",
            Panel::History => "Command History",
            Panel::Markers => "Markers",
        }
    }

    /// One instance of every panel type, for the View menu.
    pub fn all() -> Vec<Panel> {
        vec![
            Panel::TimePlot(TimePlotCfg::default()),
            Panel::XyPlot(XyPlotCfg::default()),
            Panel::Readouts(ChannelsCfg::default()),
            Panel::Indicators(ChannelsCfg::default()),
            Panel::Table(TableCfg::default()),
            Panel::Table(TableCfg {
                mode: TableMode::Log,
                filter: String::new(),
            }),
            Panel::Inspector,
            Panel::RawConsole(RawCfg::default()),
            Panel::TextConsole(TextCfg::default()),
            Panel::ChannelStats,
            Panel::LinkStats,
            Panel::Histogram(HistCfg::default()),
            Panel::CommandBar,
            Panel::Channels,
            Panel::History,
            Panel::Markers,
        ]
    }
}

/// A tab in the dock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    /// Unique id within the layout.
    pub id: u64,
    /// Title.
    pub title: String,
    /// Panel and its settings.
    pub panel: Panel,
}

/// The saved file.
#[derive(Serialize, Deserialize)]
pub struct LayoutFile {
    /// Format version.
    pub version: u32,
    /// Dock tree.
    pub dock: DockState<Tab>,
}

/// Allocates tab ids.
#[derive(Debug, Clone, Default)]
pub struct TabIds(u64);

impl TabIds {
    /// Continues after the highest id in `dock`.
    pub fn after(dock: &DockState<Tab>) -> Self {
        TabIds(dock.iter_all_tabs().map(|(_, t)| t.id).max().unwrap_or(0))
    }

    /// Makes a tab.
    pub fn tab(&mut self, panel: Panel) -> Tab {
        self.0 += 1;
        Tab {
            id: self.0,
            title: panel.title().to_string(),
            panel,
        }
    }
}

/// Default dashboard: time plot of the first channels, readouts, indicators,
/// command bar, text console and channel statistics.
pub fn default_layout(project: Option<&Project>) -> DockState<Tab> {
    let mut ids = TabIds::default();
    let mut plot = TimePlotCfg::default();
    let channels: Vec<String> = project
        .map(|p| {
            p.config
                .telemetry
                .iter()
                .flat_map(|t| t.channels.iter().cloned())
                .filter(|c| !c.contains('*'))
                .collect()
        })
        .unwrap_or_default();
    let mut indicators = ChannelsCfg::default();
    let mut readouts = ChannelsCfg::default();
    if let Some(p) = project {
        let schema = p.schema.clone();
        for c in &channels {
            let is_enum = schema
                .as_ref()
                .and_then(|s| {
                    let codec =
                        pb_core::codec::Codec::new(&p.config, s, pb_core::codec::Role::Host)
                            .ok()?;
                    let path = pb_core::schema::FieldPath::parse(c).ok()?;
                    codec
                        .rx_types()
                        .iter()
                        .find_map(|d| pb_core::schema::resolve_field(d, &path).ok())
                })
                .is_some_and(|f| {
                    matches!(
                        f.kind(),
                        prost_reflect::Kind::Enum(_) | prost_reflect::Kind::Bool
                    )
                });
            if is_enum {
                indicators.channels.push(c.clone());
            } else {
                readouts.channels.push(c.clone());
            }
        }
    }
    for c in readouts.channels.iter().take(2) {
        plot.add_channel(c);
    }
    if plot.traces.len() > 1 {
        plot.traces[1].axis = 2;
    }
    let mut dock = DockState::new(vec![ids.tab(Panel::TimePlot(plot))]);
    let surface = dock.main_surface_mut();
    let [main, left] = surface.split_left(
        NodeIndex::root(),
        0.22,
        vec![ids.tab(Panel::CommandBar), ids.tab(Panel::Channels)],
    );
    let [main, _right] = surface.split_right(
        main,
        0.72,
        vec![
            ids.tab(Panel::Readouts(readouts)),
            ids.tab(Panel::Indicators(indicators)),
        ],
    );
    let [_main, _bottom] = surface.split_below(
        main,
        0.66,
        vec![
            ids.tab(Panel::TextConsole(TextCfg::default())),
            ids.tab(Panel::ChannelStats),
            ids.tab(Panel::Table(TableCfg {
                mode: TableMode::Log,
                filter: String::new(),
            })),
            ids.tab(Panel::LinkStats),
        ],
    );
    let _ = left;
    dock
}

/// Path of the per-user working layout of a project.
pub fn local_layout_path(project: &Project) -> std::path::PathBuf {
    project.dir.join(LOCAL_LAYOUT_FILE)
}

/// True if the dock holds at least one tab.
pub fn has_tabs(dock: &DockState<Tab>) -> bool {
    dock.iter_all_tabs().next().is_some()
}

/// Where a starting layout came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutSource {
    /// `layout.local.json`.
    Local,
    /// `layout.json`.
    Project,
    /// Built-in default dashboard.
    Default,
}

/// Loads a layout file, treating a layout without tabs like a missing file.
fn load_usable(path: &Path, warnings: &mut Vec<String>) -> Option<DockState<Tab>> {
    match load_layout(path) {
        Ok(Some(d)) if has_tabs(&d) => Some(d),
        Ok(Some(_)) => {
            warnings.push(format!("{} has no panels; ignored", path.display()));
            None
        }
        Ok(None) => None,
        Err(e) => {
            warnings.push(format!("layout: {e}; ignored"));
            None
        }
    }
}

/// The project's default layout: `layout.json`, else the built-in dashboard.
pub fn project_layout(
    project: &Project,
    warnings: &mut Vec<String>,
) -> (DockState<Tab>, LayoutSource) {
    match load_usable(&project.layout_path(), warnings) {
        Some(d) => (d, LayoutSource::Project),
        None => (default_layout(Some(project)), LayoutSource::Default),
    }
}

/// The layout a project opens with: `layout.local.json`, else
/// [`project_layout`].
pub fn startup_layout(
    project: &Project,
    warnings: &mut Vec<String>,
) -> (DockState<Tab>, LayoutSource) {
    match load_usable(&local_layout_path(project), warnings) {
        Some(d) => (d, LayoutSource::Local),
        None => project_layout(project, warnings),
    }
}

/// Loads `layout.json`; `Ok(None)` if missing.
pub fn load_layout(path: &Path) -> Result<Option<DockState<Tab>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let f: LayoutFile = serde_json::from_str(&text)
        .map_err(|e| format!("{}: line {}: {e}", path.display(), e.line()))?;
    if f.version > LAYOUT_VERSION {
        return Err(format!(
            "{}: layout version {} is newer than supported ({LAYOUT_VERSION})",
            path.display(),
            f.version
        ));
    }
    Ok(Some(f.dock))
}

/// Saves `layout.json`.
pub fn save_layout(path: &Path, dock: &DockState<Tab>) -> Result<(), String> {
    let f = LayoutFile {
        version: LAYOUT_VERSION,
        dock: dock.clone(),
    };
    let mut v = serde_json::to_value(&f).map_err(|e| e.to_string())?;
    zero_null_coords(&mut v);
    let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Rects of never-shown nodes are infinite, which JSON writes as `null`.
/// They are recomputed on the next frame, so 0 is a safe stand-in.
fn zero_null_coords(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                if x.is_null() && (k == "x" || k == "y") {
                    *x = serde_json::json!(0.0);
                } else {
                    zero_null_coords(x);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(zero_null_coords),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Project {
        Project::open(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/demo-sensor"))
    }

    #[test]
    fn default_layout_has_the_dashboard_panels() {
        let dock = default_layout(Some(&demo()));
        let tabs: Vec<_> = dock.iter_all_tabs().map(|(_, t)| t.panel.clone()).collect();
        let plot = tabs.iter().find_map(|p| match p {
            Panel::TimePlot(c) => Some(c.clone()),
            _ => None,
        });
        let plot = plot.unwrap();
        assert_eq!(plot.traces[0].channel, "telemetry.vout_raw");
        assert_eq!(plot.traces[1].channel, "telemetry.current_na");
        assert!(tabs.iter().any(|p| matches!(p, Panel::Indicators(c) if c.channels == vec!["telemetry.state".to_string()])));
        for want in ["Readouts", "Commands", "Text Console", "Channel Statistics"] {
            assert!(tabs.iter().any(|p| p.title() == want), "{want}");
        }
    }

    #[test]
    fn save_load_round_trip_and_demo_file_parses() {
        let dock = default_layout(Some(&demo()));
        let p = std::env::temp_dir().join(format!("pb-layout-{}.json", std::process::id()));
        save_layout(&p, &dock).unwrap();
        let back = load_layout(&p).unwrap().unwrap();
        let a: Vec<_> = dock.iter_all_tabs().map(|(_, t)| t.clone()).collect();
        let b: Vec<_> = back.iter_all_tabs().map(|(_, t)| t.clone()).collect();
        assert_eq!(a, b);
        let _ = std::fs::remove_file(p);
        let demo_layout = demo().layout_path();
        let d = load_layout(&demo_layout)
            .unwrap()
            .expect("examples/demo-sensor/layout.json exists");
        assert!(d.iter_all_tabs().count() >= 6);
        assert!(load_layout(Path::new("/nonexistent/layout.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn startup_prefers_local_and_skips_empty_layouts() {
        let src = demo();
        let dir = std::env::temp_dir().join(format!("pb-layout-pref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        src.duplicate(&dir, "Copy").unwrap();
        let p = Project::open(&dir);
        let mut w = Vec::new();
        assert_eq!(startup_layout(&p, &mut w).1, LayoutSource::Project);

        // A working layout wins over layout.json.
        let mut ids = TabIds::default();
        let local = DockState::new(vec![ids.tab(Panel::Inspector)]);
        save_layout(&local_layout_path(&p), &local).unwrap();
        let (d, s) = startup_layout(&p, &mut w);
        assert_eq!(s, LayoutSource::Local);
        assert_eq!(d.iter_all_tabs().count(), 1);

        // Empty layouts are ignored with a warning.
        let empty: DockState<Tab> = DockState::new(Vec::new());
        save_layout(&local_layout_path(&p), &empty).unwrap();
        save_layout(&p.layout_path(), &empty).unwrap();
        let mut w = Vec::new();
        let (d, s) = startup_layout(&p, &mut w);
        assert_eq!(s, LayoutSource::Default);
        assert!(has_tabs(&d));
        assert_eq!(w.len(), 2, "{w:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ids_continue() {
        let dock = default_layout(None);
        let mut ids = TabIds::after(&dock);
        let t = ids.tab(Panel::Inspector);
        assert!(dock.iter_all_tabs().all(|(_, x)| x.id < t.id));
    }
}

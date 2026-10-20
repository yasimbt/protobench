//! Time plot, XY plot and histogram / FFT.

use std::sync::Arc;

use egui::{Color32, RichText};
use egui_plot::{
    AxisHints, Bar, BarChart, HPlacement, Legend, Line, Plot, PlotPoints, Points, VLine,
};

use pb_core::telemetry::{decimate_minmax, Channel};

use super::{not_connected, ChannelDrag};
use crate::layout::{HistCfg, TimePlotCfg, XyPlotCfg};
use crate::state::AppState;

fn store_channels(state: &AppState) -> Vec<Arc<Channel>> {
    state
        .session
        .as_ref()
        .map(|s| s.telemetry().channels())
        .unwrap_or_default()
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Value of a channel at (or just before) time `t`.
fn value_at(ch: &Channel, t: f64) -> Option<f64> {
    let d = ch.data.read();
    let i = d.ring.lower_bound(t);
    let near = [i.checked_sub(1), (i < d.ring.len()).then_some(i)];
    near.iter()
        .flatten()
        .filter_map(|&k| d.ring.get(k))
        .min_by(|a, b| (a.0 - t).abs().total_cmp(&(b.0 - t).abs()))
        .map(|(_, v)| v)
}

/// Linear map of `v` from range `from` onto range `to`.
pub fn remap(v: f64, from: (f64, f64), to: (f64, f64)) -> f64 {
    let span = from.1 - from.0;
    if span.abs() < f64::EPSILON {
        return (to.0 + to.1) / 2.0;
    }
    to.0 + (v - from.0) * (to.1 - to.0) / span
}

fn padded(lo: f64, hi: f64) -> (f64, f64) {
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0);
    }
    let span = (hi - lo).abs();
    let pad = if span < 1e-12 {
        lo.abs().max(1.0) * 0.05
    } else {
        span * 0.05
    };
    (lo - pad, hi + pad)
}

/// Multi-channel time plot.
pub fn time_plot(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut TimePlotCfg, tab_id: u64) {
    let all = store_channels(state);
    // Toolbar.
    ui.horizontal_wrapped(|ui| {
        ui.label("window");
        ui.add(
            egui::DragValue::new(&mut cfg.window_s)
                .range(0.01..=86_400.0)
                .speed(0.1)
                .suffix(" s"),
        );
        let paused = cfg.rt.frozen_at.is_some();
        if ui
            .button(if paused { "▶ Resume" } else { "⏸ Pause" })
            .clicked()
        {
            if paused {
                cfg.rt.frozen_at = None;
                cfg.rt.manual = false;
            } else {
                cfg.rt.frozen_at = Some(latest_time(cfg, &all).unwrap_or(0.0));
            }
        }
        if cfg.rt.manual
            && ui
                .button("⟲ Follow")
                .on_hover_text("Return to the live view")
                .clicked()
        {
            cfg.rt.manual = false;
            cfg.rt.frozen_at = None;
        }
        ui.checkbox(&mut cfg.autoscale, "autoscale Y");
        ui.checkbox(&mut cfg.show_picker, "channels");
        if (cfg.rt.cursor_a.is_some() || cfg.rt.cursor_b.is_some())
            && ui.button("clear cursors").clicked()
        {
            cfg.rt.cursor_a = None;
            cfg.rt.cursor_b = None;
        }
    });
    if state.session.is_none() && cfg.traces.is_empty() {
        not_connected(ui);
        return;
    }
    let mut remove = None;
    if cfg.show_picker {
        egui::Panel::left(egui::Id::new(("picker", tab_id)))
            .resizable(true)
            .default_size(200.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, t) in cfg.traces.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut t.visible, "");
                            ui.color_edit_button_srgb(&mut t.color);
                            egui::ComboBox::from_id_salt(("axis", tab_id, i))
                                .width(36.0)
                                .selected_text(format!("Y{}", t.axis))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut t.axis, 1, "Y1 (left)");
                                    ui.selectable_value(&mut t.axis, 2, "Y2 (right)");
                                });
                            if ui.small_button("✖").clicked() {
                                remove = Some(i);
                            }
                            ui.label(&t.channel);
                        });
                    }
                    ui.separator();
                    ui.weak("Tick channels (or drag them here):");
                    for c in &all {
                        let mut on = cfg.traces.iter().any(|t| t.channel == c.name);
                        if ui.checkbox(&mut on, c.label()).changed() {
                            if on {
                                cfg.add_channel(&c.name);
                            } else {
                                cfg.traces.retain(|t| t.channel != c.name);
                            }
                        }
                    }
                });
            });
    }
    if let Some(i) = remove {
        cfg.traces.remove(i);
    }

    let follow_end = match cfg.rt.frozen_at {
        Some(t) => Some(t),
        None if !cfg.rt.manual => latest_time(cfg, &all),
        None => None,
    };
    let traces: Vec<(crate::layout::Trace, Option<Arc<Channel>>)> = cfg
        .traces
        .iter()
        .map(|t| (t.clone(), all.iter().find(|c| c.name == t.channel).cloned()))
        .collect();
    let has_y2 = traces.iter().any(|(t, _)| t.axis == 2 && t.visible);
    let window = cfg.window_s;
    let autoscale = cfg.autoscale;
    let width_px = ui.available_width().max(100.0) as usize;
    let y2_range = std::rc::Rc::new(std::cell::Cell::new((0.0f64, 1.0f64)));
    let y1_range = std::rc::Rc::new(std::cell::Cell::new((0.0f64, 1.0f64)));
    let (cursor_a, cursor_b) = (cfg.rt.cursor_a, cfg.rt.cursor_b);

    let mut plot = Plot::new(("timeplot", tab_id))
        .legend(Legend::default())
        .x_axis_label("time [s]")
        .allow_double_click_reset(false);
    if has_y2 {
        let (r1, r2) = (y1_range.clone(), y2_range.clone());
        plot = plot.custom_y_axes(vec![
            AxisHints::new_y().label("Y1"),
            AxisHints::new_y()
                .label("Y2")
                .placement(HPlacement::Right)
                .formatter(move |mark, _| {
                    let v = remap(mark.value, r1.get(), r2.get());
                    pb_core::schema::format_value(v, Some("{:.3}"))
                }),
        ]);
    }
    let y1c = y1_range.clone();
    let y2c = y2_range.clone();
    let resp = plot.show(ui, move |pui| {
        if let Some(end) = follow_end {
            pui.set_plot_bounds_x((end - window)..=end);
        }
        let b = pui.plot_bounds();
        let (x0, x1) = (b.min()[0], b.max()[0]);
        let mut series: Vec<(crate::layout::Trace, String, Vec<[f64; 2]>)> = Vec::new();
        for (t, ch) in &traces {
            let Some(ch) = ch else { continue };
            if !t.visible {
                continue;
            }
            let pts = {
                let d = ch.data.read();
                decimate_minmax(&d.ring, x0, x1, width_px)
            };
            series.push((t.clone(), ch.label(), pts));
        }
        let range = |axis: u8| {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for (t, _, pts) in &series {
                if t.axis != axis {
                    continue;
                }
                for p in pts.iter().filter(|p| p[0] >= x0 && p[0] <= x1) {
                    lo = lo.min(p[1]);
                    hi = hi.max(p[1]);
                }
            }
            padded(lo, hi)
        };
        let r1 = if autoscale || !y1c.get().0.is_finite() {
            let r = range(1);
            let r = if series.iter().all(|(t, _, _)| t.axis == 2) {
                (0.0, 1.0)
            } else {
                r
            };
            pui.set_plot_bounds_y(r.0..=r.1);
            r
        } else {
            (b.min()[1], b.max()[1])
        };
        let r2 = if autoscale { range(2) } else { y2c.get() };
        y1c.set(r1);
        y2c.set(r2);
        for (t, label, pts) in series {
            let pts: Vec<[f64; 2]> = if t.axis == 2 {
                pts.into_iter()
                    .map(|p| [p[0], remap(p[1], r2, r1)])
                    .collect()
            } else {
                pts
            };
            let name = if t.axis == 2 {
                format!("{label} (Y2)")
            } else {
                label
            };
            pui.line(Line::new(name, PlotPoints::from(pts)).color(rgb(t.color)));
        }
        for (c, n) in [(cursor_a, "A"), (cursor_b, "B")] {
            if let Some(x) = c {
                pui.vline(VLine::new(format!("cursor {n}"), x).color(Color32::GRAY));
            }
        }
        pui.pointer_coordinate()
    });

    let r = &resp.response;
    if r.dragged()
        || (r.hovered()
            && ui.input(|i| i.smooth_scroll_delta != egui::Vec2::ZERO || i.zoom_delta() != 1.0))
    {
        cfg.rt.manual = true;
    }
    if r.clicked() {
        if let Some(p) = resp.inner {
            let m = ui.input(|i| i.modifiers);
            if m.shift {
                cfg.rt.cursor_a = Some(p.x);
            } else if m.command || m.ctrl {
                cfg.rt.cursor_b = Some(p.x);
            }
        }
    }
    if let Some(drag) = r.dnd_release_payload::<ChannelDrag>() {
        cfg.add_channel(&drag.0);
    }
    let bounds = resp.transform.bounds();
    let (vx0, vx1) = (bounds.min()[0], bounds.max()[0]);
    let visible: Vec<String> = cfg
        .traces
        .iter()
        .filter(|t| t.visible)
        .map(|t| t.channel.clone())
        .collect();
    r.context_menu(|ui| {
        if ui.button("Export visible data as CSV…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("CSV", &["csv"])
                .set_file_name("plot.csv")
                .save_file()
            {
                state.export_csv(
                    path,
                    visible.clone(),
                    Some((vx0, vx1)),
                    pb_core::recorder::CsvMode::Joined,
                );
            }
            ui.close();
        }
        if ui.button("Clear cursors").clicked() {
            cfg.rt.cursor_a = None;
            cfg.rt.cursor_b = None;
            ui.close();
        }
    });

    // Cursor readout.
    if cfg.rt.cursor_a.is_some() || cfg.rt.cursor_b.is_some() {
        let (a, b) = (cfg.rt.cursor_a, cfg.rt.cursor_b);
        ui.horizontal_wrapped(|ui| {
            if let Some(a) = a {
                ui.monospace(format!("A {a:.6} s"));
            }
            if let Some(b) = b {
                ui.monospace(format!("B {b:.6} s"));
            }
            if let (Some(a), Some(b)) = (a, b) {
                let dt = b - a;
                ui.monospace(format!("Δt {dt:.6} s"));
                if dt.abs() > 0.0 {
                    ui.monospace(format!("1/Δt {:.3} Hz", 1.0 / dt.abs()));
                }
            }
        });
        for t in cfg.traces.iter().filter(|t| t.visible) {
            let Some(ch) = all.iter().find(|c| c.name == t.channel) else {
                continue;
            };
            let va = a.and_then(|x| value_at(ch, x));
            let vb = b.and_then(|x| value_at(ch, x));
            let mut s = format!("{:<28}", ch.label());
            if let Some(v) = va {
                s.push_str(&format!(" A={}", ch.format_value(v)));
            }
            if let Some(v) = vb {
                s.push_str(&format!(" B={}", ch.format_value(v)));
            }
            if let (Some(x), Some(y)) = (va, vb) {
                s.push_str(&format!(" ΔY={}", ch.display.format(y - x)));
            }
            ui.label(RichText::new(s).monospace().color(rgb(t.color)));
        }
    } else {
        ui.weak("Shift+click: cursor A · Ctrl+click: cursor B · drag/scroll: pan/zoom · right-click: export");
    }
}

fn latest_time(cfg: &TimePlotCfg, all: &[Arc<Channel>]) -> Option<f64> {
    cfg.traces
        .iter()
        .filter_map(|t| all.iter().find(|c| c.name == t.channel))
        .filter_map(|c| c.last().map(|(t, _)| t))
        .fold(None, |acc: Option<f64>, t| {
            Some(acc.map_or(t, |a| a.max(t)))
        })
}

fn channel_combo(
    ui: &mut egui::Ui,
    id: (&str, u64),
    label: &str,
    value: &mut Option<String>,
    all: &[Arc<Channel>],
) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.clone().unwrap_or_else(|| format!("({label})")))
        .show_ui(ui, |ui| {
            for c in all {
                ui.selectable_value(value, Some(c.name.clone()), c.label());
            }
        });
}

/// XY plot: Y channel against X channel, paired by sample order.
pub fn xy_plot(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut XyPlotCfg, tab_id: u64) {
    let all = store_channels(state);
    ui.horizontal(|ui| {
        ui.label("X");
        channel_combo(ui, ("xy_x", tab_id), "x channel", &mut cfg.x, &all);
        ui.label("Y");
        channel_combo(ui, ("xy_y", tab_id), "y channel", &mut cfg.y, &all);
        ui.label("points");
        if cfg.points == 0 {
            cfg.points = 2000;
        }
        ui.add(egui::DragValue::new(&mut cfg.points).range(10..=1_000_000));
    });
    let find = |n: &Option<String>| {
        n.as_ref()
            .and_then(|n| all.iter().find(|c| &c.name == n).cloned())
    };
    let (Some(x), Some(y)) = (find(&cfg.x), find(&cfg.y)) else {
        ui.weak("Pick an X and a Y channel.");
        return;
    };
    let tail = |c: &Channel| -> Vec<f64> {
        let d = c.data.read();
        let n = d.ring.len();
        let start = n.saturating_sub(cfg.points);
        (start..n)
            .filter_map(|i| d.ring.get(i))
            .map(|(_, v)| v)
            .collect()
    };
    let (xs, ys) = (tail(&x), tail(&y));
    let n = xs.len().min(ys.len());
    let pts: Vec<[f64; 2]> = (0..n)
        .map(|i| [xs[xs.len() - n + i], ys[ys.len() - n + i]])
        .collect();
    let resp = Plot::new(("xy", tab_id))
        .x_axis_label(x.label())
        .y_axis_label(y.label())
        .show(ui, |pui| {
            pui.points(
                Points::new(
                    format!("{} vs {}", y.label(), x.label()),
                    PlotPoints::from(pts),
                )
                .radius(1.5),
            );
        });
    if let Some(d) = resp.response.dnd_release_payload::<ChannelDrag>() {
        if cfg.x.is_none() {
            cfg.x = Some(d.0.clone());
        } else {
            cfg.y = Some(d.0.clone());
        }
    }
}

/// Magnitude spectrum of `x` (length rounded down to a power of two).
pub fn fft_magnitude(x: &[f64]) -> Vec<f64> {
    let n = x.len().next_power_of_two() / if x.len().is_power_of_two() { 1 } else { 2 };
    if n < 2 {
        return Vec::new();
    }
    let mean = x[..n].iter().sum::<f64>() / n as f64;
    // Hann window, mean removed.
    let mut re: Vec<f64> = x[..n]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (v - mean)
                * 0.5
                * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos())
        })
        .collect();
    let mut im = vec![0.0; n];
    // Bit reversal.
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (ang * k as f64).sin_cos();
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
    (0..n / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() * 2.0 / n as f64)
        .collect()
}

/// Histogram or FFT of one channel.
pub fn histogram(ui: &mut egui::Ui, state: &mut AppState, cfg: &mut HistCfg, tab_id: u64) {
    let all = store_channels(state);
    ui.horizontal(|ui| {
        channel_combo(ui, ("hist", tab_id), "channel", &mut cfg.channel, &all);
        ui.selectable_value(&mut cfg.fft, false, "Histogram");
        ui.selectable_value(&mut cfg.fft, true, "FFT");
        if cfg.fft {
            ui.label("samples");
        } else {
            ui.label("bins");
            ui.add(egui::DragValue::new(&mut cfg.bins).range(2..=1000));
            ui.label("samples");
        }
        ui.add(egui::DragValue::new(&mut cfg.samples).range(16..=1_048_576));
    });
    let Some(ch) = cfg
        .channel
        .as_ref()
        .and_then(|n| all.iter().find(|c| &c.name == n))
        .cloned()
    else {
        ui.weak("Pick a channel.");
        return;
    };
    let samples: Vec<(f64, f64)> = {
        let d = ch.data.read();
        let n = d.ring.len();
        (n.saturating_sub(cfg.samples)..n)
            .filter_map(|i| d.ring.get(i))
            .collect()
    };
    if samples.len() < 4 {
        ui.weak("Not enough samples yet.");
        return;
    }
    if cfg.fft {
        let span = samples.last().map(|s| s.0).unwrap_or(0.0) - samples[0].0;
        let fs = if span > 0.0 {
            (samples.len() - 1) as f64 / span
        } else {
            1.0
        };
        let vals: Vec<f64> = samples.iter().map(|s| s.1).collect();
        let mag = fft_magnitude(&vals);
        let n = mag.len() * 2;
        let pts: Vec<[f64; 2]> = mag
            .iter()
            .enumerate()
            .map(|(k, m)| [k as f64 * fs / n as f64, *m])
            .collect();
        let peak = pts
            .iter()
            .skip(1)
            .max_by(|a, b| a[1].total_cmp(&b[1]))
            .copied();
        if let Some(p) = peak {
            ui.label(format!(
                "fs ≈ {fs:.1} Hz, peak {:.3} Hz (amplitude {:.4})",
                p[0], p[1]
            ));
        }
        Plot::new(("fft", tab_id))
            .x_axis_label("frequency [Hz]")
            .y_axis_label("amplitude")
            .show(ui, |pui| {
                pui.line(Line::new(ch.label(), PlotPoints::from(pts)));
            });
    } else {
        let (lo, hi) = samples
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| {
                (lo.min(s.1), hi.max(s.1))
            });
        let bins = cfg.bins.max(2);
        let w = if hi > lo {
            (hi - lo) / bins as f64
        } else {
            1.0
        };
        let mut counts = vec![0u64; bins];
        for s in &samples {
            let i = (((s.1 - lo) / w) as usize).min(bins - 1);
            counts[i] += 1;
        }
        let bars: Vec<Bar> = counts
            .iter()
            .enumerate()
            .map(|(i, c)| Bar::new(lo + (i as f64 + 0.5) * w, *c as f64).width(w * 0.95))
            .collect();
        Plot::new(("hist", tab_id))
            .x_axis_label(ch.label())
            .y_axis_label("count")
            .show(ui, |pui| {
                pui.bar_chart(BarChart::new(ch.label(), bars));
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_the_tone() {
        let fs = 1000.0;
        let x: Vec<f64> = (0..1024)
            .map(|i| (2.0 * std::f64::consts::PI * 125.0 * i as f64 / fs).sin())
            .collect();
        let m = fft_magnitude(&x);
        assert_eq!(m.len(), 512);
        let k = m
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert!(((k as f64) * fs / 1024.0 - 125.0).abs() < 1.5, "{k}");
        assert!(fft_magnitude(&[1.0]).is_empty());
    }

    #[test]
    fn remap_axes() {
        assert_eq!(remap(5.0, (0.0, 10.0), (100.0, 200.0)), 150.0);
        assert_eq!(remap(1.0, (1.0, 1.0), (0.0, 2.0)), 1.0);
        assert_eq!(padded(1.0, 1.0), (0.95, 1.05));
    }
}

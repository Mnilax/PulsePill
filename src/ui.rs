//! Layout and drawing of the island. All coordinates are DIPs inside the panel window.

use crate::gfx::*;
use crate::model::*;
use windows_sys::Win32::Graphics::GdiPlus::PointF;

pub mod pal {
    use crate::gfx::Argb;
    pub const BG: Argb = 0xFF0A0A0C;
    pub const RUNNING: Argb = 0xFF30D158;
    pub const ATTENTION: Argb = 0xFFFF9F0A;
    pub const FAILED: Argb = 0xFFFF453A;
    pub const COMPLETED: Argb = 0x73FFFFFF;
    pub const IDLE: Argb = 0x40FFFFFF;
    pub const CPU: Argb = 0xFF64D2FF;
    pub const GPU: Argb = 0xFF5E8BFF;
    pub const NET_UP: Argb = 0xFFBF5AF2;
    pub const CAUTION: Argb = 0xFFFFD60A;
    pub const PRIMARY: Argb = 0xEBFFFFFF;
    pub const SECONDARY: Argb = 0x85FFFFFF;
    pub const TERTIARY: Argb = 0x52FFFFFF;
    pub const CARD: Argb = 0x0FFFFFFF;
    pub const HAIRLINE: Argb = 0x17FFFFFF;
}
use pal::*;

pub const MARGIN: f32 = 28.0; // transparent room for the shadow
pub const TOP_GAP: f32 = 6.0;
pub const COLLAPSED_W: f32 = 196.0;
pub const COLLAPSED_H: f32 = 30.0;
pub const EXPANDED_W: f32 = 660.0;
pub const EXPANDED_H: f32 = 262.0;
pub const PANEL_W: f32 = EXPANDED_W + 2.0 * MARGIN;
pub const PANEL_H: f32 = TOP_GAP + EXPANDED_H + MARGIN;

const LEFT_W: f32 = 376.0;
const PAD: f32 = 12.0;
const HEADER_H: f32 = 40.0;

/// The island's rect inside the panel at expansion `t`.
/// `dx`: horizontal offset of the collapsed pill from the panel center (non-zero when the pill sits
/// near a screen edge and the panel was clamped on-screen). `up`: grow upwards (pill in the lower
/// half of the screen), keeping the bottom edge fixed.
pub fn shape_rect(t: f32, dx: f32, up: bool) -> Rect {
    let w = COLLAPSED_W + (EXPANDED_W - COLLAPSED_W) * t;
    let h = COLLAPSED_H + (EXPANDED_H - COLLAPSED_H) * t;
    let cx = PANEL_W / 2.0 + dx * (1.0 - t.clamp(0.0, 1.0));
    let y = if up { PANEL_H - TOP_GAP - h } else { TOP_GAP };
    Rect::new(cx - w / 2.0, y, w, h)
}

/// A `w`×`h` rect centered horizontally in `shape`, pinned to its growth edge.
pub fn pinned(shape: Rect, w: f32, h: f32, up: bool) -> Rect {
    let y = if up { shape.bottom() - h } else { shape.y };
    Rect::new(shape.x + (shape.w - w) / 2.0, y, w, h)
}

fn state_color(s: AgentState) -> Argb {
    match s {
        AgentState::Running => RUNNING,
        AgentState::NeedsInput => ATTENTION,
        AgentState::Failed => FAILED,
        AgentState::Completed => COMPLETED,
        AgentState::Idle => IDLE,
    }
}

fn pressure_color(p: f64) -> Argb {
    if p < 0.6 {
        RUNNING
    } else if p < 0.85 {
        CAUTION
    } else {
        FAILED
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Hit {
    Collapsed,
    OpenT3,
    Settings,
    Thread(String),
    Agents, // scrollable list area
}

pub struct View<'a> {
    pub agents: &'a AgentsSnapshot,
    pub system: &'a SystemSnapshot,
    pub now: i64,
    pub hover: Option<Hit>,
    pub scroll: f32,
    pub tile: Tile,
}

pub struct Frame {
    pub hits: Vec<(Rect, Hit)>,
    pub content_h: f32,
    pub list_h: f32,
}

fn orb(g: &Gfx, cx: f32, cy: f32, d: f32, color: Argb, glow: bool) {
    if glow {
        g.fill_circle(cx, cy, d * 2.2, with_alpha(color, 0.18));
    }
    g.fill_circle(cx, cy, d, color);
}

fn capsule(g: &Gfx, r: Rect, fill: Argb) {
    g.fill_round(r, r.h / 2.0, fill);
}

/// Text chip with padding; returns its width. `right` anchors the chip's right edge.
fn chip(g: &Gfx, text: &str, x: f32, cy: f32, size: f32, fg: Argb, bg: Argb, right: bool) -> f32 {
    let w = g.measure(text, size, Weight::Semibold) + 12.0;
    let x0 = if right { x - w } else { x };
    let r = Rect::new(x0, cy - 8.0, w, 16.0);
    capsule(g, r, bg);
    g.text(text, r, size, Weight::Semibold, fg, Align::Center);
    w
}

fn sparkline(g: &Gfx, r: Rect, values: &[f64], color: Argb, width: f32, head: bool) {
    if values.len() < 2 {
        g.line(r.x, r.bottom() - 1.0, r.right(), r.bottom() - 1.0, 1.0, with_alpha(color, 0.35));
        return;
    }
    let n = values.len();
    let step = r.w / (n - 1) as f32;
    let pts: Vec<PointF> = values
        .iter()
        .enumerate()
        .map(|(i, v)| PointF { X: r.x + i as f32 * step, Y: r.bottom() - (*v as f32).clamp(0.0, 1.0) * (r.h - width) - width / 2.0 })
        .collect();
    let mut area = pts.clone();
    area.push(PointF { X: r.right(), Y: r.bottom() });
    area.push(PointF { X: r.x, Y: r.bottom() });
    g.fill_area(&area, r.y, r.bottom(), with_alpha(color, 0.30));
    g.polyline(&pts, width, color);
    if head {
        let last = pts[n - 1];
        g.fill_circle(last.X, last.Y, 6.0, with_alpha(color, 0.25));
        g.fill_circle(last.X, last.Y, 3.2, color);
    }
}

pub fn draw_background(g: &Gfx, shape: Rect, t: f32) {
    let radius = 15.0 + 9.0 * t;
    // Soft shadow: a few stacked translucent rounded rects (cheap, no blur pass).
    let layers = if t > 0.02 { 6 } else { 2 };
    for i in (1..=layers).rev() {
        let spread = i as f32 * (2.0 + 2.5 * t);
        let r = Rect::new(shape.x - spread, shape.y - spread * 0.4, shape.w + spread * 2.0, shape.h + spread * 1.6);
        g.fill_round(r, radius + spread, with_alpha(0xFF000000, (0.05 + 0.03 * t) * (1.0 - i as f32 / (layers as f32 + 1.0))));
    }
    g.fill_round(shape, radius, BG);
    g.stroke_round(shape.inset(0.5, 0.5), radius - 0.5, HAIRLINE, 1.0);
}

// MARK: - Collapsed

pub fn draw_collapsed(g: &Gfx, v: &View, shape: Rect) -> Vec<(Rect, Hit)> {
    let cy = shape.y + shape.h / 2.0;
    let a = v.agents;
    // Left wing: agent counters.
    let mut x = shape.x + 12.0;
    if a.needs_input_count > 0 {
        let label = a.needs_input_count.to_string();
        let w = g.measure(&label, 12.0, Weight::Semibold) + 26.0;
        let r = Rect::new(x - 4.0, cy - 9.5, w, 19.0);
        capsule(g, r, with_alpha(ATTENTION, 0.18));
        orb(g, x + 6.0, cy, 7.0, ATTENTION, false);
        g.text(&label, Rect::new(x + 13.0, cy - 9.0, w, 18.0), 12.0, Weight::Semibold, ATTENTION, Align::Left);
        x += w + 4.0;
    }
    if a.running_count > 0 {
        orb(g, x + 4.0, cy, 7.0, RUNNING, true);
        g.text(&a.running_count.to_string(), Rect::new(x + 12.0, cy - 9.0, 40.0, 18.0), 12.0, Weight::Semibold, PRIMARY, Align::Left);
    }
    if a.needs_input_count == 0 && a.running_count == 0 {
        let (c, label) = if a.server_online { (IDLE, "idle") } else { (0x33FFFFFF, "T3 off") };
        orb(g, x + 4.0, cy, 7.0, c, false);
        g.text(label, Rect::new(x + 13.0, cy - 9.0, 60.0, 18.0), 12.0, Weight::Semibold, TERTIARY, Align::Left);
    }

    // Right wing: CPU sparkline + value (or a low-battery warning).
    let s = v.system;
    let right = shape.right() - 12.0;
    let low_battery = matches!(s.battery_level, Some(l) if l <= 0.2 && !s.is_plugged_in);
    let (text, color) = if low_battery {
        (format!("bat {}", fmt_percent(s.battery_level.unwrap())), FAILED)
    } else {
        (fmt_percent(s.cpu_usage), PRIMARY)
    };
    g.text(&text, Rect::new(right - 60.0, cy - 9.0, 60.0, 18.0), 12.0, Weight::Semibold, color, Align::Right);
    let tw = g.measure(&text, 12.0, Weight::Semibold);
    let hist = &s.cpu_history[s.cpu_history.len().saturating_sub(20)..];
    sparkline(g, Rect::new(right - tw - 34.0, cy - 6.0, 26.0, 12.0), hist, CPU, 1.25, false);
    vec![(shape, Hit::Collapsed)]
}

// MARK: - Expanded

pub fn draw_expanded(g: &Gfx, v: &View, shape: Rect) -> Frame {
    let mut hits = Vec::new();
    let a = v.agents;
    let hy = shape.y + 14.0;
    let hcy = hy + 11.0;

    // Header: "Agents" + counters, T3 button on the right of the left column.
    let mut x = shape.x + PAD + 6.0;
    g.text("Agents", Rect::new(x, hy, 80.0, 22.0), 13.5, Weight::Bold, PRIMARY, Align::Left);
    x += g.measure("Agents", 13.5, Weight::Bold) + 9.0;
    if a.running_count > 0 {
        x += chip(g, &format!("{} running", a.running_count), x, hcy, 10.5, RUNNING, with_alpha(RUNNING, 0.14), false) + 6.0;
    }
    if a.needs_input_count > 0 {
        chip(g, &format!("{} waiting", a.needs_input_count), x, hcy, 10.5, ATTENTION, with_alpha(ATTENTION, 0.14), false);
    }
    let left_right = shape.x + PAD + LEFT_W;
    let label = if a.server_online { "T3 Code  ↗" } else { "T3 offline  ↗" };
    let bw = g.measure(label, 11.0, Weight::Semibold) + 30.0;
    let br = Rect::new(left_right - bw - 4.0, hcy - 10.5, bw, 21.0);
    let hov = v.hover == Some(Hit::OpenT3);
    capsule(g, br, if hov { 0x1AFFFFFF } else { 0x0DFFFFFF });
    orb(g, br.x + 12.0, hcy, 5.0, if a.server_online { RUNNING } else { 0x40FFFFFF }, false);
    g.text(label, Rect::new(br.x + 20.0, br.y, bw - 20.0, br.h), 11.0, Weight::Semibold, if hov { PRIMARY } else { SECONDARY }, Align::Left);
    hits.push((br, Hit::OpenT3));

    // System header.
    let sx = left_right + PAD;
    let sw = shape.right() - PAD - sx;
    g.text("This PC", Rect::new(sx + 4.0, hy, 120.0, 22.0), 13.5, Weight::Bold, PRIMARY, Align::Left);
    // Settings gear in the top-right corner, with the app's own memory next to it.
    let gr = Rect::new(sx + sw - 22.0, hcy - 11.0, 22.0, 22.0);
    let ghov = v.hover == Some(Hit::Settings);
    if ghov {
        g.fill_circle(gr.x + 11.0, hcy, 22.0, 0x1AFFFFFF);
    }
    gear(g, gr.x + 11.0, hcy, if ghov { PRIMARY } else { SECONDARY }, if ghov { 0xFF212123 } else { BG });
    hits.push((gr, Hit::Settings));
    let mem = format!("app {} MB", v.system.self_memory / (1024 * 1024));
    g.text(&mem, Rect::new(sx, hy, sw - 28.0, 22.0), 10.0, Weight::Semibold, TERTIARY, Align::Right);

    // Agents list.
    let list = Rect::new(shape.x + PAD, shape.y + HEADER_H, LEFT_W, shape.h - HEADER_H - PAD);
    let content_h = draw_agents(g, v, list, &mut hits);
    hits.push((list, Hit::Agents));

    // System tiles.
    draw_system(g, v.system, v.tile, Rect::new(sx, list.y, sw, list.h));
    Frame { hits, content_h, list_h: list.h }
}

/// A small cog: a ring with eight teeth.
fn gear(g: &Gfx, cx: f32, cy: f32, color: Argb, hole: Argb) {
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (s, c) = a.sin_cos();
        g.line(cx + c * 4.0, cy + s * 4.0, cx + c * 6.6, cy + s * 6.6, 2.6, color);
    }
    g.fill_circle(cx, cy, 10.5, color);
    g.fill_circle(cx, cy, 4.2, hole);
}

fn row_height(t: &AgentThread) -> f32 {
    if t.state == AgentState::NeedsInput && t.pending_request_summary.is_some() { 60.0 } else { 44.0 }
}

fn draw_agents(g: &Gfx, v: &View, list: Rect, hits: &mut Vec<(Rect, Hit)>) -> f32 {
    let a = v.agents;
    if a.threads.is_empty() {
        g.fill_round(list, 14.0, 0x09FFFFFF);
        let cy = list.y + list.h / 2.0;
        g.text("No agents running", Rect::new(list.x, cy - 22.0, list.w, 20.0), 13.0, Weight::Semibold, SECONDARY, Align::Center);
        let sub = if a.server_online {
            "Agents you start in T3 Code show up here.".to_string()
        } else {
            a.last_error.clone().unwrap_or_else(|| "T3 Code server is offline".into())
        };
        g.text(&sub, Rect::new(list.x + 12.0, cy, list.w - 24.0, 18.0), 11.0, Weight::Regular, TERTIARY, Align::Center);
        return 0.0;
    }
    g.clip(list);
    let mut y = list.y - v.scroll;
    for t in &a.threads {
        let h = row_height(t);
        let r = Rect::new(list.x, y, list.w, h - 2.0);
        if r.bottom() > list.y && r.y < list.bottom() {
            let hovered = v.hover.as_ref() == Some(&Hit::Thread(t.id.clone()));
            draw_row(g, t, r, v.now, hovered);
            hits.push((r, Hit::Thread(t.id.clone())));
        }
        y += h;
    }
    g.reset_clip();
    let total = y + v.scroll - list.y;
    // Bottom fade hint when the list scrolls.
    if total > list.h + 1.0 && v.scroll + list.h < total - 1.0 {
        for i in 0..8 {
            let fy = list.bottom() - 14.0 + i as f32 * 1.75;
            g.fill_round(Rect::new(list.x, fy, list.w, 1.75), 0.0, with_alpha(BG, 0.12 * i as f32));
        }
    }
    total
}

fn draw_row(g: &Gfx, t: &AgentThread, r: Rect, now: i64, hovered: bool) {
    let needs = t.state == AgentState::NeedsInput;
    if needs {
        g.fill_round(r, 11.0, with_alpha(ATTENTION, if hovered { 0.17 } else { 0.11 }));
        g.stroke_round(r.inset(0.25, 0.25), 11.0, with_alpha(ATTENTION, 0.22), 0.5);
    } else if hovered {
        g.fill_round(r, 11.0, 0x12FFFFFF);
    }
    let x = r.x + 10.0;
    let ty = r.y + 5.0;
    let icy = ty + 9.0;
    match t.state {
        AgentState::Completed => {
            let c = COMPLETED;
            g.line(x + 1.0, icy, x + 3.5, icy + 2.6, 1.6, c);
            g.line(x + 3.5, icy + 2.6, x + 8.0, icy - 2.8, 1.6, c);
        }
        AgentState::Failed => {
            g.line(x + 1.5, icy - 3.0, x + 7.5, icy + 3.0, 1.6, FAILED);
            g.line(x + 7.5, icy - 3.0, x + 1.5, icy + 3.0, 1.6, FAILED);
        }
        s => orb(g, x + 4.5, icy, 7.0, state_color(s), matches!(s, AgentState::Running | AgentState::NeedsInput)),
    }

    let tx = x + 20.0;
    let time = match (t.state, t.run_started_at) {
        (AgentState::Running, Some(s)) => fmt_elapsed(s, now),
        _ => fmt_relative(t.updated_at, now),
    };
    let time_color = if t.state == AgentState::Running { with_alpha(RUNNING, 0.85) } else { TERTIARY };
    g.text(&time, Rect::new(r.right() - 60.0, ty, 50.0, 18.0), 10.5, Weight::Semibold, time_color, Align::Right);
    let tw = g.measure(&time, 10.5, Weight::Semibold);
    let title_color = if matches!(t.state, AgentState::Completed | AgentState::Idle) { with_alpha(PRIMARY, 0.78) } else { PRIMARY };
    g.text(&t.title, Rect::new(tx, ty, r.right() - 18.0 - tw - tx, 18.0), 12.5, Weight::Semibold, title_color, Align::Left);

    let mut ly = ty + 18.0;
    if needs {
        if let Some(sum) = &t.pending_request_summary {
            g.text(sum, Rect::new(tx, ly, r.right() - 10.0 - tx, 16.0), 11.0, Weight::Semibold, ATTENTION, Align::Left);
            ly += 16.0;
        }
    }
    // Secondary line + model / subagent chips on the right.
    let chip_cy = ly + 8.0;
    let mut cx = r.right() - 10.0;
    cx -= chip(g, &model_chip(&t.provider, t.model.as_deref()), cx, chip_cy, 9.5, SECONDARY, 0x12FFFFFF, true) + 5.0;
    if t.subagent_count > 0 {
        cx -= chip(g, &format!("{} sub", t.subagent_count), cx, chip_cy, 9.5, with_alpha(RUNNING, 0.9), with_alpha(RUNNING, 0.12), true) + 5.0;
    }
    let preview = t.last_message_preview.as_deref().unwrap_or("").trim();
    let secondary = match (&t.project_name, preview.is_empty()) {
        (Some(p), false) => format!("{p} · {preview}"),
        (Some(p), true) => p.clone(),
        (None, false) => preview.to_string(),
        (None, true) => match t.state {
            AgentState::Running => "Working…",
            AgentState::NeedsInput => "Waiting for you",
            AgentState::Completed => "Completed",
            AgentState::Failed => "Failed",
            AgentState::Idle => "Idle",
        }
        .into(),
    };
    let sc = if t.state == AgentState::Failed { with_alpha(FAILED, 0.8) } else { SECONDARY };
    g.text(&secondary, Rect::new(tx, ly, cx - 4.0 - tx, 16.0), 11.0, Weight::Regular, sc, Align::Left);
}

// MARK: - System tiles

fn tile(g: &Gfx, r: Rect, title: &str) {
    g.fill_round(r, 12.0, CARD);
    g.text(&title.to_uppercase(), Rect::new(r.x + 10.0, r.y + 6.0, r.w - 20.0, 13.0), 9.0, Weight::Bold, TERTIARY, Align::Left);
}

fn meter(g: &Gfx, r: Rect, fraction: f64, color: Argb) {
    capsule(g, r, 0x1AFFFFFF);
    let w = (r.w * fraction.clamp(0.0, 1.0) as f32).max(r.h);
    capsule(g, Rect::new(r.x, r.y, w, r.h), color);
}

fn big_value(g: &Gfx, x: f32, y: f32, value: &str, unit: &str, size: f32, color: Argb) {
    g.text(value, Rect::new(x, y, 120.0, size + 6.0), size, Weight::Semibold, color, Align::Left);
    let w = g.measure(value, size, Weight::Semibold);
    let us = (size * 0.55).max(10.0);
    let gap = if unit.starts_with(|c: char| c.is_alphanumeric()) { 6.0 } else { 2.0 };
    g.text(unit, Rect::new(x + w + gap,y + (size - us) * 0.62, 90.0, us + 6.0), us, Weight::Semibold, TERTIARY, Align::Left);
}

fn draw_system(g: &Gfx, s: &SystemSnapshot, tile_kind: Tile, r: Rect) {
    let gap = 8.0;
    let small = ((r.h - 2.0 * gap) * 0.31).round();
    let large = r.h - 2.0 * gap - 2.0 * small;
    let half = (r.w - gap) / 2.0;

    // CPU (spans the top).
    let cpu = Rect::new(r.x, r.y, r.w, large);
    tile(g, cpu, "CPU");
    big_value(g, cpu.x + 10.0, cpu.bottom() - 36.0, &format!("{}", (s.cpu_usage * 100.0).round() as i64), "%", 24.0, PRIMARY);
    let hist = &s.cpu_history[s.cpu_history.len().saturating_sub(40)..];
    sparkline(g, Rect::new(cpu.x + 76.0, cpu.y + 22.0, cpu.w - 90.0, cpu.h - 32.0), hist, CPU, 1.5, true);

    let row1 = r.y + large + gap;
    let row2 = row1 + small + gap;

    // Memory.
    let m = Rect::new(r.x, row1, half, small);
    tile(g, m, "Memory");
    let mc = pressure_color(s.memory_load);
    g.fill_circle(m.right() - 13.0, m.y + 12.5, 5.0, mc);
    big_value(g, m.x + 10.0, m.y + 19.0, &fmt_gb(s.memory_used, 1), &format!("/ {} GB", fmt_gb(s.memory_total, 0)), 16.0, PRIMARY);
    meter(g, Rect::new(m.x + 10.0, m.bottom() - 11.0, m.w - 20.0, 4.0), s.memory_load, mc);

    // GPU.
    let gr = Rect::new(r.x + half + gap, row1, half, small);
    tile(g, gr, "GPU");
    match s.gpu_usage {
        Some(u) => {
            big_value(g, gr.x + 10.0, gr.y + 19.0, &format!("{}", (u * 100.0).round() as i64), "%", 16.0, PRIMARY);
            let hist = &s.gpu_history[s.gpu_history.len().saturating_sub(24)..];
            sparkline(g, Rect::new(gr.x + 54.0, gr.y + 22.0, gr.w - 64.0, gr.h - 30.0), hist, GPU, 1.25, false);
        }
        None => g.text("n/a", Rect::new(gr.x + 10.0, gr.y + 19.0, 60.0, 22.0), 16.0, Weight::Semibold, TERTIARY, Align::Left),
    }

    // Configurable tile: battery, disk space, disk activity or uptime.
    draw_flex_tile(g, s, tile_kind.resolve(s.battery_level.is_some()), Rect::new(r.x, row2, half, small));

    // Network.
    let n = Rect::new(r.x + half + gap, row2, half, small);
    tile(g, n, "Network");
    let line = |y: f32, arrow: &str, color: Argb, v: f64| {
        g.text(arrow, Rect::new(n.x + 10.0, y, 14.0, 16.0), 11.0, Weight::Bold, color, Align::Left);
        g.text(&fmt_rate(v), Rect::new(n.x + 24.0, y, n.w - 32.0, 16.0), 11.5, Weight::Semibold, PRIMARY, Align::Left);
    };
    line(n.y + 20.0, "↓", CPU, s.net_down);
    line(n.y + 36.0, "↑", NET_UP, s.net_up);
}

fn draw_flex_tile(g: &Gfx, s: &SystemSnapshot, kind: Tile, b: Rect) {
    let note_rect = Rect::new(b.x + 10.0, b.bottom() - 19.0, b.w - 20.0, 14.0);
    match kind {
        Tile::Battery | Tile::Auto => {
            tile(g, b, "Battery");
            match s.battery_level {
                Some(level) => {
                    let color = if level <= 0.2 && !s.is_plugged_in { FAILED } else if s.is_charging { RUNNING } else { PRIMARY };
                    let note = if s.is_charging {
                        "charging".to_string()
                    } else if s.is_plugged_in {
                        "plugged in".to_string()
                    } else {
                        s.battery_minutes.map(fmt_duration).unwrap_or_default()
                    };
                    big_value(g, b.x + 10.0, b.y + 19.0, &fmt_percent(level), &note, 16.0, color);
                    meter(g, Rect::new(b.x + 10.0, b.bottom() - 11.0, b.w - 20.0, 4.0), level, if color == PRIMARY { RUNNING } else { color });
                }
                None => {
                    g.text("AC power", Rect::new(b.x + 10.0, b.y + 19.0, b.w - 20.0, 22.0), 14.0, Weight::Semibold, PRIMARY, Align::Left);
                    g.text("no battery", note_rect, 10.0, Weight::Regular, TERTIARY, Align::Left);
                }
            }
        }
        Tile::DiskSpace => {
            tile(g, b, &format!("Disk {}", s.disk_drive.trim_end_matches(':')));
            if s.disk_total == 0 {
                g.text("n/a", Rect::new(b.x + 10.0, b.y + 19.0, 60.0, 22.0), 16.0, Weight::Semibold, TERTIARY, Align::Left);
                return;
            }
            let load = s.disk_used as f64 / s.disk_total as f64;
            let color = pressure_color((load - 0.15).max(0.0)); // disks are fine fuller than RAM
            g.fill_circle(b.right() - 13.0, b.y + 12.5, 5.0, color);
            let (used, total, unit) = if s.disk_total >= 1 << 40 {
                (format!("{:.1}", s.disk_used as f64 / (1u64 << 40) as f64), format!("{:.1}", s.disk_total as f64 / (1u64 << 40) as f64), "TB")
            } else {
                (fmt_gb(s.disk_used, 0), fmt_gb(s.disk_total, 0), "GB")
            };
            big_value(g, b.x + 10.0, b.y + 19.0, &used, &format!("/ {total} {unit}"), 16.0, PRIMARY);
            meter(g, Rect::new(b.x + 10.0, b.bottom() - 11.0, b.w - 20.0, 4.0), load, color);
        }
        Tile::DiskActivity => {
            tile(g, b, "Disk");
            match s.disk_active {
                Some(a) => {
                    let pct = format!("{}% active", (a * 100.0).round() as i64);
                    g.text(&pct, Rect::new(b.x + 10.0, b.y + 6.0, b.w - 20.0, 13.0), 9.0, Weight::Bold, SECONDARY, Align::Right);
                    let line = |y: f32, label: &str, color: Argb, v: f64| {
                        g.text(label, Rect::new(b.x + 10.0, y, 14.0, 16.0), 10.0, Weight::Bold, color, Align::Left);
                        g.text(&fmt_rate(v), Rect::new(b.x + 24.0, y, b.w - 32.0, 16.0), 11.5, Weight::Semibold, PRIMARY, Align::Left);
                    };
                    line(b.y + 20.0, "R", CPU, s.disk_read);
                    line(b.y + 36.0, "W", NET_UP, s.disk_write);
                }
                None => g.text("starting…", Rect::new(b.x + 10.0, b.y + 20.0, b.w - 20.0, 16.0), 11.5, Weight::Semibold, TERTIARY, Align::Left),
            }
        }
        Tile::Uptime => {
            tile(g, b, "Uptime");
            big_value(g, b.x + 10.0, b.y + 19.0, &fmt_uptime(s.uptime_secs), "", 16.0, PRIMARY);
            g.text("since last restart", note_rect, 10.0, Weight::Regular, TERTIARY, Align::Left);
        }
    }
}

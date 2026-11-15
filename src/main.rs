//! PulsePill: a Dynamic-Island-style pill for Windows that shows T3 Code threads and PC
//! vitals. A Windows port of NotchPulse (macOS). One layered Win32 window, GDI+ drawing.
#![windows_subsystem = "windows"]

mod gfx;
mod mock;
mod model;
mod sqlite;
mod sys;
mod t3;
mod ui;

use model::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, ReleaseCapture, SetCapture, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APP_DATA: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;
const TIMER_TRACK: usize = 1;
const TIMER_ANIM: usize = 2;
const HOTKEY_TOGGLE: i32 = 1;
const ANIM_SECS: f32 = 0.24;
const EXPAND_DELAY: Duration = Duration::from_millis(140);
const COLLAPSE_DELAY: Duration = Duration::from_millis(380);
const DRAG_THRESHOLD: i32 = 5;
const SNAP_DISTANCE: i32 = 40;
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const SETTINGS_KEY: &str = "Software\\PulsePill";
const APP_NAME: &str = "PulsePill";
const ID_TILE: usize = 100;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[derive(Default)]
struct Shared {
    agents: Option<AgentsSnapshot>,
    system: Option<SystemSnapshot>,
}

struct Options {
    mock: Option<String>,
    expanded: bool,
    gpu: bool,
}

struct Drag {
    start: POINT,
    anchor: (i32, i32),
    moved: bool,
}

struct App {
    hwnd: HWND,
    shared: Arc<Mutex<Shared>>,
    opts: Options,
    agents: AgentsSnapshot,
    system: SystemSnapshot,
    // DIB backing store
    mem_dc: HDC,
    dib: HBITMAP,
    bits: *mut u8,
    px: (i32, i32),
    scale: f32,
    // placement
    /// User-chosen screen position of the collapsed pill's top-center (None = default spot).
    anchor: Option<(i32, i32)>,
    /// Effective (clamped) anchor after the last `place()`.
    placed: (i32, i32),
    origin: (i32, i32),
    dx: f32,
    up: bool,
    // interaction
    t: f32,
    target: f32,
    last_frame: Instant,
    hover_since: Option<Instant>,
    outside_since: Option<Instant>,
    hover: Option<ui::Hit>,
    hits: Vec<(gfx::Rect, ui::Hit)>,
    scroll: f32,
    max_scroll: f32,
    drag: Option<Drag>,
    /// After a drop the cursor still sits on the pill; don't expand until it leaves.
    no_expand: bool,
    // visibility
    user_hidden: bool,
    fullscreen: bool,
    shown: bool,
    tray_icon: HICON,
    tick: u64,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}
static mut TASKBAR_CREATED: u32 = 0;

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn ease_out(t: f32) -> f32 {
    // Slight overshoot, like a soft spring.
    let c1 = 1.2f32;
    let c3 = c1 + 1.0;
    let x = t - 1.0;
    1.0 + c3 * x * x * x + c1 * x * x
}

fn cursor_pos() -> POINT {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    p
}

fn monitor_rects(pt: POINT) -> (RECT, RECT) {
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        (mi.rcMonitor, mi.rcWork)
    }
}

impl App {
    fn default_anchor(&self) -> (i32, i32) {
        let (m, _) = monitor_rects(POINT { x: 0, y: 0 }); // primary monitor
        (m.left + (m.right - m.left) / 2, m.top + (ui::TOP_GAP * self.scale).round() as i32)
    }

    fn place(&mut self) {
        unsafe {
            let dpi = GetDpiForWindow(self.hwnd).max(96);
            self.scale = dpi as f32 / 96.0;
        }
        let s = self.scale;
        let px = ((ui::PANEL_W * s).ceil() as i32, (ui::PANEL_H * s).ceil() as i32);
        let on_screen = |a: (i32, i32)| unsafe { !MonitorFromPoint(POINT { x: a.0, y: a.1 }, MONITOR_DEFAULTTONULL).is_null() };
        let anchor = self.anchor.filter(|&a| on_screen(a)).unwrap_or_else(|| self.default_anchor());
        let (_, work) = monitor_rects(POINT { x: anchor.0, y: anchor.1 });
        let (cw, ch) = ((ui::COLLAPSED_W * s).round() as i32, (ui::COLLAPSED_H * s).round() as i32);
        // Keep the collapsed pill fully inside the work area.
        let ax = anchor.0.clamp(work.left + cw / 2, (work.right - cw / 2).max(work.left + cw / 2));
        let ay = anchor.1.clamp(work.top, (work.bottom - ch).max(work.top));
        self.placed = (ax, ay);
        self.up = ay + ch / 2 > (work.top + work.bottom) / 2;
        // Clamp the panel horizontally so the expanded island stays on screen.
        let ox = (ax - px.0 / 2).clamp(work.left, (work.right - px.0).max(work.left));
        self.dx = (ax - (ox + px.0 / 2)) as f32 / s;
        let oy = if self.up { ay + ch - ((ui::PANEL_H - ui::TOP_GAP) * s).round() as i32 } else { ay - (ui::TOP_GAP * s).round() as i32 };
        self.origin = (ox, oy);
        if px != self.px {
            unsafe {
                if !self.dib.is_null() {
                    DeleteObject(self.dib as HGDIOBJ);
                }
                let mut bmi: BITMAPINFO = std::mem::zeroed();
                bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = px.0;
                bmi.bmiHeader.biHeight = -px.1; // top-down
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                let mut bits = std::ptr::null_mut();
                self.dib = CreateDIBSection(self.mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
                self.bits = bits as *mut u8;
                SelectObject(self.mem_dc, self.dib as HGDIOBJ);
            }
            self.px = px;
        }
        self.render();
    }

    fn shape(&self) -> gfx::Rect {
        ui::shape_rect(ease_out(self.t).max(0.0), self.dx, self.up)
    }

    fn render(&mut self) {
        if self.bits.is_null() || !self.shown {
            return;
        }
        let now = now_ms();
        let view = ui::View { agents: &self.agents, system: &self.system, now, hover: self.hover.clone(), scroll: self.scroll, tile: current_tile() };
        let shape = self.shape();
        unsafe {
            let mut g = gfx::Gfx::new(self.bits, self.px.0, self.px.1, self.scale);
            g.clear();
            ui::draw_background(&g, shape, self.t);
            if self.t < 0.4 {
                g.alpha = 1.0 - self.t / 0.4;
                ui::draw_collapsed(&g, &view, ui::pinned(shape, ui::COLLAPSED_W, ui::COLLAPSED_H, self.up));
                self.hits = vec![(shape, ui::Hit::Collapsed)];
            }
            if self.t > 0.55 {
                g.alpha = ((self.t - 0.55) / 0.45).min(1.0);
                let frame = ui::draw_expanded(&g, &view, ui::pinned(shape, ui::EXPANDED_W, ui::EXPANDED_H, self.up));
                self.max_scroll = (frame.content_h - frame.list_h).max(0.0);
                if self.t >= 1.0 {
                    self.hits = frame.hits;
                }
            }
            drop(g);
            let size = SIZE { cx: self.px.0, cy: self.px.1 };
            let pos = POINT { x: self.origin.0, y: self.origin.1 };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(self.hwnd, std::ptr::null_mut(), &pos, &size, self.mem_dc, &src, 0, &blend, ULW_ALPHA);
        }
    }

    fn set_target(&mut self, target: f32) {
        let target = if self.opts.expanded { 1.0 } else { target };
        if self.target == target {
            return;
        }
        self.target = target;
        if target == 0.0 {
            self.hover = None;
        }
        self.last_frame = Instant::now();
        unsafe { SetTimer(self.hwnd, TIMER_ANIM, 15, None) };
    }

    fn animate(&mut self) {
        let dt = self.last_frame.elapsed().as_secs_f32();
        self.last_frame = Instant::now();
        let step = dt / ANIM_SECS;
        self.t = if self.target > self.t { (self.t + step).min(self.target) } else { (self.t - step * 1.4).max(self.target) };
        if self.t == self.target {
            unsafe { KillTimer(self.hwnd, TIMER_ANIM) };
        }
        self.render();
    }

    /// Cursor position in panel DIPs.
    fn cursor(&self) -> (f32, f32) {
        let p = cursor_pos();
        ((p.x - self.origin.0) as f32 / self.scale, (p.y - self.origin.1) as f32 / self.scale)
    }

    fn hit_at(&self, x: f32, y: f32) -> Option<ui::Hit> {
        // Specific targets first (rows, button), then the scroll area / pill.
        self.hits.iter().find(|(r, h)| r.contains(x, y) && *h != ui::Hit::Agents && *h != ui::Hit::Collapsed).map(|(_, h)| h.clone())
    }

    /// Runs on a 50 ms timer while the cursor is near the island: hover intent + leave detection.
    fn track(&mut self) {
        if self.drag.is_some() {
            return;
        }
        let (x, y) = self.cursor();
        let inside = self.shape().inset(-2.0, -2.0).contains(x, y);
        if inside {
            self.outside_since = None;
            if self.target == 0.0 && !self.no_expand {
                let since = *self.hover_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= EXPAND_DELAY {
                    self.set_target(1.0);
                }
            }
            let hover = if self.t >= 1.0 { self.hit_at(x, y) } else { None };
            if hover != self.hover {
                self.hover = hover;
                self.render();
            }
        } else {
            self.hover_since = None;
            self.no_expand = false;
            if self.hover.take().is_some() {
                self.render();
            }
            if self.target > 0.0 {
                let since = *self.outside_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= COLLAPSE_DELAY {
                    self.set_target(0.0);
                }
            } else {
                unsafe { KillTimer(self.hwnd, TIMER_TRACK) };
            }
        }
    }

    // MARK: drag

    fn press(&mut self) {
        self.drag = Some(Drag { start: cursor_pos(), anchor: self.placed, moved: false });
        unsafe { SetCapture(self.hwnd) };
    }

    fn drag_move(&mut self) {
        let Some(d) = self.drag.as_mut() else { return };
        let p = cursor_pos();
        let (ddx, ddy) = (p.x - d.start.x, p.y - d.start.y);
        if !d.moved && ddx.abs().max(ddy.abs()) < DRAG_THRESHOLD {
            return;
        }
        let anchor = (d.anchor.0 + ddx, d.anchor.1 + ddy);
        if !d.moved {
            d.moved = true;
            // Drag the compact pill, not the whole panel.
            unsafe { KillTimer(self.hwnd, TIMER_ANIM) };
            self.t = 0.0;
            self.target = 0.0;
            self.hover = None;
        }
        self.anchor = Some(anchor);
        self.place();
    }

    fn release(&mut self) {
        unsafe { ReleaseCapture() };
        let Some(d) = self.drag.take() else { return };
        if !d.moved {
            self.click();
            return;
        }
        // Dropped near the default spot: snap back to it.
        let def = self.default_anchor();
        if (self.placed.0 - def.0).abs() < SNAP_DISTANCE && (self.placed.1 - def.1).abs() < SNAP_DISTANCE {
            self.anchor = None;
        } else {
            self.anchor = Some(self.placed);
        }
        save_anchor(self.anchor);
        self.no_expand = true;
        self.place();
        if self.opts.expanded {
            self.target = -1.0;
            self.set_target(1.0);
        }
    }

    // MARK: visibility

    fn update_visibility(&mut self) {
        let show = !self.user_hidden && !self.fullscreen;
        if show == self.shown {
            return;
        }
        self.shown = show;
        unsafe { ShowWindow(self.hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE }) };
        if show {
            self.t = if self.opts.expanded { 1.0 } else { 0.0 };
            self.target = self.t;
            self.place();
        }
    }

    fn toggle_hidden(&mut self) {
        self.user_hidden = !self.user_hidden;
        self.update_visibility();
    }

    fn on_data(&mut self) {
        let (a, s) = {
            let mut sh = self.shared.lock().unwrap();
            (sh.agents.take(), sh.system.take())
        };
        if let Some(a) = a {
            self.agents = a;
        }
        if let Some(s) = s {
            self.system = s;
        }
        if let Some(sc) = self.opts.mock.clone() {
            let mem = self.system.self_memory;
            self.system = mock::system(self.tick, &sc);
            self.system.self_memory = mem;
        }
        self.scroll = self.scroll.min(self.max_scroll);
        // Step aside for full-screen apps (games, video, presentations).
        let mut state = 0;
        self.fullscreen = unsafe { SHQueryUserNotificationState(&mut state) } == 0 && matches!(state, 2..=4);
        self.update_visibility();
        if self.shown {
            if self.tick % 5 == 0 {
                unsafe { SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
            }
            self.render();
        }
        self.tick += 1;
    }

    fn click(&mut self) {
        let (x, y) = self.cursor();
        if self.t < 1.0 {
            self.set_target(1.0);
            return;
        }
        match self.hit_at(x, y) {
            Some(ui::Hit::Thread(id)) => {
                let url = match &self.agents.environment_id {
                    Some(env) => format!("t3code://threads/{env}/{id}"),
                    None => "t3code://".into(),
                };
                shell_open(&url);
                self.set_target(0.0);
            }
            Some(ui::Hit::Settings) => self.menu(),
            Some(ui::Hit::OpenT3) => {
                open_t3_app();
                self.set_target(0.0);
            }
            _ => {}
        }
    }

    fn menu(&mut self) {
        unsafe {
            let m = CreatePopupMenu();
            let auto = autostart_enabled();
            let flag = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
            let hide_label = if self.user_hidden { "Show island\tCtrl+Alt+N" } else { "Hide island\tCtrl+Alt+N" };
            AppendMenuW(m, MF_STRING, 5, w(hide_label).as_ptr());
            AppendMenuW(m, MF_STRING | flag(self.opts.expanded), 4, w("Keep expanded").as_ptr());
            AppendMenuW(m, MF_STRING | if self.anchor.is_none() { MF_GRAYED } else { 0 }, 6, w("Reset position").as_ptr());
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(m, MF_STRING, 2, w("Open T3 Code").as_ptr());
            let tiles = CreatePopupMenu();
            let cur = current_tile();
            for (i, t) in Tile::ALL.iter().enumerate() {
                AppendMenuW(tiles, MF_STRING | flag(*t == cur), ID_TILE + i, w(t.label()).as_ptr());
            }
            AppendMenuW(m, MF_POPUP, tiles as usize, w("Bottom-left tile").as_ptr());
            AppendMenuW(m, MF_STRING | flag(auto), 1, w("Start with Windows").as_ptr());
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            let label = format!("PulsePill · {} MB RAM", self.system.self_memory / (1024 * 1024));
            AppendMenuW(m, MF_STRING | MF_GRAYED, 0, w(&label).as_ptr());
            AppendMenuW(m, MF_STRING, 3, w("Quit").as_ptr());
            let p = cursor_pos();
            SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, 0, self.hwnd, std::ptr::null());
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(m); // also destroys the submenu
            match cmd as usize {
                c if (ID_TILE..ID_TILE + Tile::ALL.len()).contains(&c) => {
                    let t = Tile::ALL[c - ID_TILE];
                    set_current_tile(t);
                    set_setting("Tile", Some(t.key()));
                    self.render();
                }
                1 => set_autostart(!auto),
                2 => open_t3_app(),
                3 => {
                    DestroyWindow(self.hwnd);
                }
                4 => {
                    self.opts.expanded = !self.opts.expanded;
                    let t = if self.opts.expanded { 1.0 } else { 0.0 };
                    self.target = -1.0;
                    self.set_target(t);
                }
                5 => self.toggle_hidden(),
                6 => {
                    self.anchor = None;
                    save_anchor(None);
                    self.place();
                }
                _ => {}
            }
        }
    }
}

// MARK: - Settings & shell helpers

fn set_setting(name: &str, value: Option<&str>) {
    unsafe {
        let (k, n) = (w(SETTINGS_KEY), w(name));
        match value {
            Some(v) => {
                let v = w(v);
                RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, v.as_ptr() as *const _, (v.len() * 2) as u32);
            }
            None => {
                RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr());
            }
        }
    }
}

fn save_anchor(anchor: Option<(i32, i32)>) {
    set_setting("Anchor", anchor.map(|(x, y)| format!("{x},{y}")).as_deref());
}

fn load_anchor() -> Option<(i32, i32)> {
    let v = reg_string(HKEY_CURRENT_USER, SETTINGS_KEY, "Anchor")?;
    let (x, y) = v.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn shell_open(target: &str) {
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), w("open").as_ptr(), w(target).as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
    }
}

/// Launches T3 Code via the exe registered for the `t3code://` protocol.
fn open_t3_app() {
    let cmd = reg_string(HKEY_CURRENT_USER, "Software\\Classes\\t3code\\shell\\open\\command", "")
        .or_else(|| reg_string(HKEY_CLASSES_ROOT, "t3code\\shell\\open\\command", ""));
    let exe = cmd.and_then(|c| {
        let c = c.trim();
        if let Some(rest) = c.strip_prefix('"') { rest.split('"').next().map(str::to_string) } else { c.split(' ').next().map(str::to_string) }
    });
    match exe {
        Some(exe) => shell_open(&exe),
        None => shell_open("t3code://"),
    }
}

fn reg_string(root: HKEY, key: &str, name: &str) -> Option<String> {
    unsafe {
        let (k, n) = (w(key), w(name));
        let mut size = 0u32;
        if RegGetValueW(root, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) != 0 {
            return None;
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        if RegGetValueW(root, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size) != 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

fn autostart_enabled() -> bool {
    reg_string(HKEY_CURRENT_USER, RUN_KEY, APP_NAME).is_some()
}

fn autostart_command() -> String {
    format!("\"{}\"", std::env::current_exe().unwrap_or_default().display())
}

/// If the exe was moved after "Start with Windows" was turned on, point the entry at the new path.
fn refresh_autostart() {
    if let Some(cmd) = reg_string(HKEY_CURRENT_USER, RUN_KEY, APP_NAME) {
        if !cmd.eq_ignore_ascii_case(&autostart_command()) {
            set_autostart(true);
        }
    }
}

/// `--uninstall`: remove everything PulsePill wrote to the registry. Delete the exe afterwards.
fn uninstall() {
    set_autostart(false);
    unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, w(SETTINGS_KEY).as_ptr()) };
    unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, w(SETTINGS_KEY).as_ptr()) };
}

fn set_autostart(on: bool) {
    unsafe {
        let (k, n) = (w(RUN_KEY), w(APP_NAME));
        if on {
            let v = w(&autostart_command());
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, v.as_ptr() as *const _, (v.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr());
        }
    }
}

// MARK: - Tray icon

/// Draws a 32×32 tray icon: a tiny black pill with a green status dot.
unsafe fn make_icon() -> HICON {
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = 32;
    bmi.bmiHeader.biHeight = -32;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    let mut bits = std::ptr::null_mut();
    let color = CreateDIBSection(std::ptr::null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    {
        let g = gfx::Gfx::new(bits as *mut u8, 32, 32, 1.0);
        g.clear();
        let pill = gfx::Rect::new(1.0, 8.0, 30.0, 16.0);
        g.fill_round(pill, 8.0, ui::pal::BG);
        g.stroke_round(pill.inset(0.5, 0.5), 7.5, 0x66FFFFFF, 1.0);
        g.fill_circle(9.5, 16.0, 7.0, ui::pal::RUNNING);
        g.fill_round(gfx::Rect::new(16.0, 14.5, 10.0, 3.0), 1.5, 0xCCFFFFFF);
    }
    let mask_bits = [0u8; 32 * 32 / 8];
    let mask = CreateBitmap(32, 32, 1, 1, mask_bits.as_ptr() as *const _);
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(mask as HGDIOBJ);
    DeleteObject(color as HGDIOBJ);
    icon
}

fn tray(hwnd: HWND, icon: HICON, op: NOTIFY_ICON_MESSAGE) {
    tray_with_info(hwnd, icon, op, None);
}

/// `info`: (title, text) shown as a Windows notification next to the tray icon.
fn tray_with_info(hwnd: HWND, icon: HICON, op: NOTIFY_ICON_MESSAGE, info: Option<(&str, &str)>) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = icon;
        let tip: Vec<u16> = "PulsePill — click to hide/show, right-click for menu".encode_utf16().collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        if let Some((title, text)) = info {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
            nid.hBalloonIcon = icon;
            let (t, x): (Vec<u16>, Vec<u16>) = (title.encode_utf16().collect(), text.encode_utf16().collect());
            nid.szInfoTitle[..t.len().min(63)].copy_from_slice(&t[..t.len().min(63)]);
            nid.szInfo[..x.len().min(255)].copy_from_slice(&x[..x.len().min(255)]);
        }
        Shell_NotifyIconW(op, &nid);
    }
}

// MARK: - Window procedure

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == TASKBAR_CREATED && msg != 0 {
        // Explorer restarted: the tray icon is gone, add it back.
        with_app(|a| tray(a.hwnd, a.tray_icon, NIM_ADD));
        return 0;
    }
    match msg {
        WM_APP_DATA => {
            with_app(|a| a.on_data());
            0
        }
        WM_APP_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP => {
                    with_app(|a| a.toggle_hidden());
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    with_app(|a| a.menu());
                }
                _ => {}
            }
            0
        }
        WM_HOTKEY => {
            if wp as i32 == HOTKEY_TOGGLE {
                with_app(|a| a.toggle_hidden());
            }
            0
        }
        WM_MOUSEMOVE => {
            with_app(|a| {
                if a.drag.is_some() {
                    a.drag_move();
                } else {
                    SetTimer(a.hwnd, TIMER_TRACK, 50, None);
                    a.track();
                }
            });
            0
        }
        WM_TIMER => {
            with_app(|a| match wp {
                TIMER_TRACK => a.track(),
                TIMER_ANIM => a.animate(),
                _ => {}
            });
            0
        }
        WM_LBUTTONDOWN => {
            with_app(|a| a.press());
            0
        }
        WM_LBUTTONUP => {
            with_app(|a| a.release());
            0
        }
        WM_CAPTURECHANGED => {
            // Capture stolen mid-drag (e.g. Alt+Tab): keep wherever the pill is now.
            with_app(|a| {
                if let Some(d) = a.drag.take() {
                    if d.moved {
                        a.anchor = Some(a.placed);
                        save_anchor(a.anchor);
                    }
                }
            });
            0
        }
        WM_RBUTTONUP => {
            with_app(|a| a.menu());
            0
        }
        WM_MBUTTONUP => {
            with_app(|a| a.toggle_hidden());
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xFFFF) as u16 as i16 as f32;
            with_app(|a| {
                if a.t >= 1.0 && a.max_scroll > 0.0 {
                    a.scroll = (a.scroll - delta / 120.0 * 44.0).clamp(0.0, a.max_scroll);
                    a.render();
                }
            });
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
            with_app(|a| a.place());
            0
        }
        WM_DESTROY => {
            with_app(|a| tray(a.hwnd, a.tray_icon, NIM_DELETE));
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn spawn_worker(hwnd: HWND, shared: Arc<Mutex<Shared>>, opts: &Options) {
    let hwnd = hwnd as usize;
    let mock = opts.mock.clone();
    let gpu = opts.gpu;
    std::thread::spawn(move || {
        let mut sampler = sys::Sampler::new(gpu && mock.is_none());
        let mut monitor = t3::Monitor::new();
        if let Some(sc) = &mock {
            shared.lock().unwrap().agents = Some(mock::agents(sc));
        }
        loop {
            let system = sampler.sample();
            let agents = if mock.is_none() { monitor.poll() } else { None };
            {
                let mut sh = shared.lock().unwrap();
                sh.system = Some(system);
                if agents.is_some() {
                    sh.agents = agents;
                }
            }
            unsafe { PostMessageW(hwnd as HWND, WM_APP_DATA, 0, 0) };
            std::thread::sleep(Duration::from_millis(1000));
        }
    });
}

/// `--dump`: print what the island would show, then exit (debugging without UI).
fn dump() {
    let mut m = t3::Monitor::new();
    let started = Instant::now();
    let snap = m.poll().unwrap_or_default();
    println!("server_online={} running={} needs_input={} error={:?} ({} ms)", snap.server_online, snap.running_count, snap.needs_input_count, snap.last_error, started.elapsed().as_millis());
    let now = now_ms();
    for t in &snap.threads {
        println!(
            "{:<10} {:<5} {:<12} {} | {} | {}",
            format!("{:?}", t.state),
            fmt_relative(t.updated_at, now),
            model_chip(&t.provider, t.model.as_deref()),
            t.title,
            t.pending_request_summary.as_deref().unwrap_or(""),
            t.last_message_preview.as_deref().unwrap_or("")
        );
    }
    if let Ok(conn) = sqlite::Connection::open_read_only(&t3::Paths::resolve().database.to_string_lossy()) {
        let times: Vec<u128> = (0..3)
            .map(|_| {
                let t = Instant::now();
                let _ = t3::read(&conn, true);
                t.elapsed().as_millis()
            })
            .collect();
        println!("warm read: {times:?} ms");
    }
    let mut s = sys::Sampler::new(true);
    s.sample();
    std::thread::sleep(Duration::from_millis(1000));
    let s = s.sample();
    println!(
        "cpu={} gpu={:?} mem={}/{} GB bat={:?} plugged={} down={} up={} disk {}={}/{} GB uptime={}",
        fmt_percent(s.cpu_usage),
        s.gpu_usage.map(fmt_percent),
        fmt_gb(s.memory_used, 1),
        fmt_gb(s.memory_total, 0),
        s.battery_level.map(fmt_percent),
        s.is_plugged_in,
        fmt_rate(s.net_down),
        fmt_rate(s.net_up),
        s.disk_drive,
        fmt_gb(s.disk_used, 0),
        fmt_gb(s.disk_total, 0),
        fmt_uptime(s.uptime_secs)
    );
}

/// `--render-png=<file>`: draws the expanded pill with sample data into a transparent PNG
/// (README screenshots without capturing whatever else is on screen).
fn render_png(path: &str, tile: Tile) {
    let scale = 2.0;
    let (w, h) = ((ui::PANEL_W * scale).ceil() as i32, (ui::PANEL_H * scale).ceil() as i32);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let agents = mock::agents("busy");
    let mut system = mock::system(7, "busy");
    system.self_memory = 11 * 1024 * 1024;
    let view = ui::View { agents: &agents, system: &system, now: now_ms(), hover: None, scroll: 0.0, tile };
    gfx::startup();
    unsafe {
        let g = gfx::Gfx::new(buf.as_mut_ptr(), w, h, scale);
        g.clear();
        let shape = ui::shape_rect(1.0, 0.0, false);
        ui::draw_background(&g, shape, 1.0);
        ui::draw_expanded(&g, &view, shape);
        if !g.save_png(path) {
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--dump") {
        dump();
        return;
    }
    if args.iter().any(|a| a == "--uninstall") {
        uninstall();
        return;
    }
    let tile_arg = args.iter().find_map(|a| a.strip_prefix("--tile=")).and_then(Tile::from_key);
    if let Some(path) = args.iter().find_map(|a| a.strip_prefix("--render-png=")) {
        render_png(path, tile_arg.unwrap_or_default());
        return;
    }
    let mut opts = Options { mock: None, expanded: false, gpu: true };
    let mut start_hidden = false;
    for a in &args {
        match a.as_str() {
            "--mock" => opts.mock = Some("busy".into()),
            "--expanded" => opts.expanded = true,
            "--no-gpu" => opts.gpu = false,
            "--hidden" => start_hidden = true,
            s if s.starts_with("--mock-scenario=") => opts.mock = Some(s["--mock-scenario=".len()..].into()),
            _ => {}
        }
    }

    unsafe {
        // Single instance.
        CreateMutexW(std::ptr::null(), 1, w("Local\\PulsePill.SingleInstance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        gfx::startup();
        TASKBAR_CREATED = RegisterWindowMessageW(w("TaskbarCreated").as_ptr());

        let hinst = GetModuleHandleW(std::ptr::null());
        let class = w("PulsePillIsland");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            w("PulsePill").as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        let tray_icon = make_icon();
        tray(hwnd, tray_icon, NIM_ADD);
        if let Some(t) = tile_arg.or_else(|| reg_string(HKEY_CURRENT_USER, SETTINGS_KEY, "Tile").and_then(|v| Tile::from_key(&v))) {
            set_current_tile(t);
        }
        refresh_autostart();
        // First launch: say where the app lives and how to get rid of it.
        if reg_string(HKEY_CURRENT_USER, SETTINGS_KEY, "Welcomed").is_none() {
            set_setting("Welcomed", Some("1"));
            tray_with_info(
                hwnd,
                tray_icon,
                NIM_MODIFY,
                Some((
                    "PulsePill is running",
                    "Hide it: click the tray icon or press Ctrl+Alt+N. Right-click the pill or the icon for settings, Start with Windows and Quit.",
                )),
            );
        }
        // Global show/hide shortcut; silently skipped if another app owns it.
        RegisterHotKey(hwnd, HOTKEY_TOGGLE, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'N' as u32);

        let shared = Arc::new(Mutex::new(Shared::default()));
        let expanded = opts.expanded;
        spawn_worker(hwnd, shared.clone(), &opts);
        let app = App {
            hwnd,
            shared,
            opts,
            agents: AgentsSnapshot::default(),
            system: SystemSnapshot::default(),
            mem_dc: CreateCompatibleDC(std::ptr::null_mut()),
            dib: std::ptr::null_mut(),
            bits: std::ptr::null_mut(),
            px: (0, 0),
            scale: 1.0,
            anchor: load_anchor(),
            placed: (0, 0),
            origin: (0, 0),
            dx: 0.0,
            up: false,
            t: if expanded { 1.0 } else { 0.0 },
            target: if expanded { 1.0 } else { 0.0 },
            last_frame: Instant::now(),
            hover_since: None,
            outside_since: None,
            hover: None,
            hits: Vec::new(),
            scroll: 0.0,
            max_scroll: 0.0,
            drag: None,
            no_expand: false,
            user_hidden: start_hidden,
            fullscreen: false,
            shown: false,
            tray_icon,
            tick: 0,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| {
            a.place();
            a.update_visibility();
        });

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

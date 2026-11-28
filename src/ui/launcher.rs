// language: Rust, file: src/ui/launcher.rs, target: Linux (Wayland & X11)
//! Native Glassmorphism Launcher UI for Linux, matching the macOS SwiftUI launcher.

use crate::{
    notifications::{
        fallback_relay::NotificationEngine,
        ipc::{DaemonToHostMessage, HostToDaemonMessage},
        linux_bluez::{inspect_bluez_status, solicit_ancs_advertisement},
        linux_desktop::LinuxDesktopNotifier,
        IosNotification, NotificationSettings,
    },
    transport::device_scanner::TransportPreference,
    ui::metal_renderer::MetalRenderer,
};
use ab_glyph_rasterizer::{point, Rasterizer};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use ttf_parser::{Face, OutlineBuilder};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{CursorIcon, Window, WindowId},
};

const WINDOW_WIDTH: f32 = 860.0;
const WINDOW_HEIGHT: f32 = 560.0;
const MIN_WIDTH: f32 = 780.0;
const MIN_HEIGHT: f32 = 520.0;
const SIDEBAR_WIDTH: f32 = 268.0;
const OUTER_PAD: f32 = 18.0;
const TOP_PAD: f32 = 20.0;
const PANEL_GAP: f32 = 16.0;
const GITHUB_RELEASES_API: &str =
    "https://api.github.com/repos/arifianilhamnrr/iphone-mirror/releases/latest";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherSettings {
    #[serde(default)]
    pub transport: TransportPreference,
    #[serde(default = "default_true")]
    pub auto_rotate: bool,
    #[serde(default)]
    pub auto_connect: bool,
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
    #[serde(default)]
    pub notifications: NotificationSettings,
}

fn default_true() -> bool {
    true
}

impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            transport: TransportPreference::Auto,
            auto_rotate: true,
            auto_connect: false,
            auto_check_updates: true,
            notifications: NotificationSettings::default(),
        }
    }
}

fn settings_file_path() -> Result<PathBuf> {
    let base = if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = env::var_os("HOME").context("HOME is not set")?;
        PathBuf::from(home).join(".config")
    };
    Ok(base.join("iphone-mirror").join("settings.json"))
}

fn load_launcher_settings() -> LauncherSettings {
    settings_file_path()
        .ok()
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_launcher_settings(settings: &LauncherSettings) {
    if let Ok(path) = settings_file_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(settings) {
            let _ = fs::write(path, bytes);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LauncherDevice {
    pub udid: String,
    pub connection_type: String,
    pub device_id: u32,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

impl LauncherDevice {
    pub fn id(&self) -> String {
        format!("{}-{}", self.udid, self.connection_type)
    }

    pub fn is_usb(&self) -> bool {
        self.connection_type == "usb"
    }

    pub fn is_network(&self) -> bool {
        self.connection_type == "network"
    }

    pub fn is_remote(&self) -> bool {
        self.host.is_some() && self.port.is_some()
    }

    pub fn connection_label(&self) -> &'static str {
        if self.is_usb() {
            "USB"
        } else if self.is_network() {
            "Wi-Fi"
        } else {
            "Other"
        }
    }

    fn is_uuid_like(value: &str) -> bool {
        let stripped: String = value.chars().filter(|c| *c != '-' && *c != ':').collect();
        stripped.len() >= 16 && stripped.chars().all(|c| c.is_ascii_hexdigit())
    }

    pub fn resolved_user_name(&self) -> String {
        if let Some(name) = &self.name {
            let trimmed = name.trim();
            if !trimmed.is_empty() && !Self::is_uuid_like(trimmed) {
                return trimmed.to_string();
            }
        }
        if let Some(host) = &self.host {
            let trimmed = host.trim_matches('.');
            let short = trimmed.strip_suffix(".local").unwrap_or(trimmed);
            if !short.is_empty() && !Self::is_uuid_like(short) {
                return short.to_string();
            }
        }
        if self.is_usb() {
            "iPhone (USB)".to_string()
        } else {
            "iPhone (Wi-Fi)".to_string()
        }
    }

    pub fn display_name(&self) -> String {
        let user = self.resolved_user_name();
        if let Some(model) = &self.model {
            let m = model.trim();
            if !m.is_empty() {
                return format!("{user} · {m}");
            }
        }
        user
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearbyDevice {
    pub name: String,
    pub service: String,
}

impl NearbyDevice {
    pub fn display_name(&self) -> String {
        let compact: String = self.name.chars().filter(|c| *c != '-').collect();
        if compact.len() >= 24 && compact.chars().all(|c| c.is_ascii_hexdigit()) {
            "Nearby iPhone".to_string()
        } else {
            self.name.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

pub fn parse_semver(tag: &str) -> Vec<u32> {
    let cleaned = tag
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('-')
        .next()
        .unwrap_or("");
    cleaned
        .split('.')
        .take(3)
        .map(|part| part.parse::<u32>().unwrap_or(0))
        .collect()
}

pub fn is_newer_version(remote_tag: &str, local_version: &str, local_is_beta: bool) -> bool {
    let remote = parse_semver(remote_tag);
    let local = parse_semver(local_version);
    let count = remote.len().max(local.len()).max(3);
    for i in 0..count {
        let r = remote.get(i).copied().unwrap_or(0);
        let l = local.get(i).copied().unwrap_or(0);
        if r != l {
            return r > l;
        }
    }
    let lower = remote_tag.to_ascii_lowercase();
    let remote_pre = lower.contains("beta") || lower.contains("alpha") || lower.contains("rc");
    local_is_beta && !remote_pre
}

pub fn filter_devices_for_transport(
    devices: &[LauncherDevice],
    transport: TransportPreference,
) -> Vec<LauncherDevice> {
    let mut seen_udids = BTreeSet::new();
    let mut seen_hosts = BTreeSet::new();
    let mut seen_names = BTreeSet::new();
    let mut out = Vec::new();
    for d in devices {
        let matches = match transport {
            TransportPreference::Auto | TransportPreference::Airplay => true,
            TransportPreference::Usb => d.is_usb(),
            TransportPreference::Wifi => d.is_network(),
        };
        if !matches {
            continue;
        }
        if !seen_udids.insert(d.udid.clone()) {
            continue;
        }
        if let Some(host) = &d.host {
            let host_key = host.trim_end_matches('.').to_ascii_lowercase();
            if !host_key.is_empty() && !seen_hosts.insert(host_key) {
                continue;
            }
        }
        if matches!(
            transport,
            TransportPreference::Auto | TransportPreference::Airplay
        ) {
            let name_key = d.resolved_user_name().to_ascii_lowercase();
            if !seen_names.insert(name_key) {
                continue;
            }
        }
        out.push(d.clone());
    }
    out
}

enum LauncherEvent {
    DevicesScanned {
        manual: bool,
        result: Result<Vec<LauncherDevice>, String>,
    },
    NearbyScanned(Vec<NearbyDevice>),
    UpdateChecked {
        silent: bool,
        result: Result<Option<AvailableUpdate>, String>,
        status: String,
    },
    BackendExited {
        exit_ok: bool,
        error_detail: Option<String>,
    },
    PairFinished(Result<String, String>),
    ScanRequested,
    BluezStatusUpdated(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HitAction {
    Refresh,
    OpenSettings,
    CloseSettings,
    ToggleNotificationsPanel,
    ToggleNotificationsEnabled,
    ToggleMuteWhileMirroring,
    ToggleHidePreviews,
    SendTestNotification,
    SolicitBluezAncs,
    ClearNotifications,
    SelectMode(TransportPreference),
    SelectDevice(String),
    PrimaryAction,
    PairWifi,
    CheckOrOpenUpdate,
    DownloadUpdate,
    DismissUpdate,
    ToggleAutoConnect,
    ToggleAutoRotate,
    ToggleAutoCheckUpdates,
    SettingsCheckUpdates,
    SettingsSelectTransport(TransportPreference),
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

struct HitRegion {
    rect: Rect,
    action: HitAction,
}

#[derive(Debug, Clone, Copy)]
struct ColorRgba {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl ColorRgba {
    const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    const fn white(a: f32) -> Self {
        Self {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a,
        }
    }

    const fn black(a: f32) -> Self {
        Self {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a,
        }
    }

    fn with_alpha(self, alpha: f32) -> Self {
        Self {
            a: (self.a * alpha).clamp(0.0, 1.0),
            ..self
        }
    }

    fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a + (other.a - self.a) * t,
        }
    }

    fn to_xrgb(self) -> u32 {
        let r = (self.r.clamp(0.0, 1.0) * 255.0).round() as u32;
        let g = (self.g.clamp(0.0, 1.0) * 255.0).round() as u32;
        let b = (self.b.clamp(0.0, 1.0) * 255.0).round() as u32;
        0xFF00_0000 | (r << 16) | (g << 8) | b
    }

    fn blend_pixel(dst: u32, src: Self) -> u32 {
        let a = src.a.clamp(0.0, 1.0);
        if a <= 0.001 {
            return dst;
        }
        if a >= 0.999 {
            return src.to_xrgb();
        }
        let dr = ((dst >> 16) & 0xFF) as f32 / 255.0;
        let dg = ((dst >> 8) & 0xFF) as f32 / 255.0;
        let db = (dst & 0xFF) as f32 / 255.0;
        let inv = 1.0 - a;
        let r = ((src.r * a + dr * inv).clamp(0.0, 1.0) * 255.0).round() as u32;
        let g = ((src.g * a + dg * inv).clamp(0.0, 1.0) * 255.0).round() as u32;
        let b = ((src.b * a + db * inv).clamp(0.0, 1.0) * 255.0).round() as u32;
        0xFF00_0000 | (r << 16) | (g << 8) | b
    }
}

// ============================================================================
// Pure-Rust Font Loader & Glyph Cache (ttf-parser + ab_glyph_rasterizer)
// ============================================================================

struct RasterizedGlyph {
    width: usize,
    height: usize,
    offset_x: i32,
    offset_y: i32,
    advance_px: f32,
    bitmap: Vec<u8>,
}

struct ContourSink {
    rasterizer: Rasterizer,
    scale: f32,
    x_shift: f32,
    y_shift: f32,
    start: ab_glyph_rasterizer::Point,
    last: ab_glyph_rasterizer::Point,
    has_point: bool,
}

impl ContourSink {
    fn map(&self, x: f32, y: f32) -> ab_glyph_rasterizer::Point {
        point(x * self.scale + self.x_shift, self.y_shift - y * self.scale)
    }
}

impl OutlineBuilder for ContourSink {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.map(x, y);
        self.start = p;
        self.last = p;
        self.has_point = true;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.map(x, y);
        if self.has_point {
            self.rasterizer.draw_line(self.last, p);
        }
        self.last = p;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let p1 = self.map(x1, y1);
        let p2 = self.map(x, y);
        if self.has_point {
            self.rasterizer.draw_quad(self.last, p1, p2);
        }
        self.last = p2;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let p1 = self.map(x1, y1);
        let p2 = self.map(x2, y2);
        let p3 = self.map(x, y);
        if self.has_point {
            self.rasterizer.draw_cubic(self.last, p1, p2, p3);
        }
        self.last = p3;
    }

    fn close(&mut self) {
        if self.has_point && (self.last.x != self.start.x || self.last.y != self.start.y) {
            self.rasterizer.draw_line(self.last, self.start);
        }
        self.last = self.start;
    }
}

struct FontRenderer {
    regular_data: Option<Vec<u8>>,
    bold_data: Option<Vec<u8>>,
    cache: HashMap<(char, u16, bool), Arc<RasterizedGlyph>>,
}

impl FontRenderer {
    fn new() -> Self {
        let regular_data = Self::load_font_bytes(false);
        let bold_data = Self::load_font_bytes(true).or_else(|| regular_data.clone());
        Self {
            regular_data,
            bold_data,
            cache: HashMap::new(),
        }
    }

    fn load_font_bytes(bold: bool) -> Option<Vec<u8>> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(home) = env::var_os("HOME") {
            let user_fonts = PathBuf::from(home).join(".local/share/fonts");
            if bold {
                candidates.push(user_fonts.join("SF-Pro-Display-Semibold.otf"));
                candidates.push(user_fonts.join("SF-Pro-Display-Bold.otf"));
                candidates.push(user_fonts.join("SF-Pro-Text-Semibold.otf"));
                candidates.push(user_fonts.join("Inter-SemiBold.ttf"));
                candidates.push(user_fonts.join("Inter-Bold.ttf"));
            } else {
                candidates.push(user_fonts.join("SF-Pro-Display-Regular.otf"));
                candidates.push(user_fonts.join("SF-Pro-Text-Regular.otf"));
                candidates.push(user_fonts.join("Inter-Regular.ttf"));
            }
        }

        if let Some(fc_path) = Self::fc_match_path(bold) {
            candidates.push(fc_path);
        }

        let system_paths = if bold {
            [
                "/usr/share/fonts/noto/NotoSans-Bold.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
                "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
                "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
            ]
        } else {
            [
                "/usr/share/fonts/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            ]
        };
        for p in system_paths {
            candidates.push(PathBuf::from(p));
        }

        for path in candidates {
            if let Ok(bytes) = fs::read(&path) {
                if Face::parse(&bytes, 0).is_ok() {
                    return Some(bytes);
                }
            }
        }
        None
    }

    fn fc_match_path(bold: bool) -> Option<PathBuf> {
        let pattern = if bold {
            "sans-serif:weight=bold"
        } else {
            "sans-serif"
        };
        let output = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", pattern])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }

    fn get_glyph(&mut self, ch: char, px_size: f32, bold: bool) -> Option<Arc<RasterizedGlyph>> {
        let key_px = px_size.round().clamp(6.0, 200.0) as u16;
        let key = (ch, key_px, bold);
        if let Some(cached) = self.cache.get(&key) {
            return Some(cached.clone());
        }
        let data = if bold {
            self.bold_data.as_deref()
        } else {
            self.regular_data.as_deref()
        }?;
        let face = Face::parse(data, 0).ok()?;
        let units_per_em = face.units_per_em() as f32;
        if units_per_em <= 0.0 {
            return None;
        }
        let scale = (key_px as f32) / units_per_em;
        let glyph_id = face.glyph_index(ch).or_else(|| face.glyph_index('?'))?;
        let advance_px = face
            .glyph_hor_advance(glyph_id)
            .map(|a| a as f32 * scale)
            .unwrap_or(key_px as f32 * 0.5);

        let Some(bbox) = face.glyph_bounding_box(glyph_id) else {
            let empty = Arc::new(RasterizedGlyph {
                width: 0,
                height: 0,
                offset_x: 0,
                offset_y: 0,
                advance_px,
                bitmap: Vec::new(),
            });
            self.cache.insert(key, empty.clone());
            return Some(empty);
        };

        let min_x = (bbox.x_min as f32 * scale).floor() as i32;
        let max_x = (bbox.x_max as f32 * scale).ceil() as i32;
        let min_y = (-bbox.y_max as f32 * scale).floor() as i32;
        let max_y = (-bbox.y_min as f32 * scale).ceil() as i32;
        let width = ((max_x - min_x) + 2).max(1) as usize;
        let height = ((max_y - min_y) + 2).max(1) as usize;
        let x_shift = (-min_x + 1) as f32;
        let y_shift = (-min_y + 1) as f32;

        let mut sink = ContourSink {
            rasterizer: Rasterizer::new(width, height),
            scale,
            x_shift,
            y_shift,
            start: point(0.0, 0.0),
            last: point(0.0, 0.0),
            has_point: false,
        };
        let _ = face.outline_glyph(glyph_id, &mut sink);
        let mut bitmap = vec![0_u8; width * height];
        sink.rasterizer.for_each_pixel(|idx, alpha| {
            if idx < bitmap.len() {
                bitmap[idx] = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });

        let glyph = Arc::new(RasterizedGlyph {
            width,
            height,
            offset_x: min_x - 1,
            offset_y: min_y - 1,
            advance_px,
            bitmap,
        });
        self.cache.insert(key, glyph.clone());
        Some(glyph)
    }

    fn measure_text(&mut self, text: &str, px_size: f32, bold: bool) -> f32 {
        let mut width = 0.0_f32;
        for ch in text.chars() {
            if let Some(g) = self.get_glyph(ch, px_size, bold) {
                width += g.advance_px;
            } else {
                width += px_size * 0.56;
            }
        }
        width
    }

    fn truncate_to_width(&mut self, text: &str, px_size: f32, bold: bool, max_w: f32) -> String {
        if self.measure_text(text, px_size, bold) <= max_w {
            return text.to_string();
        }
        let ellipsis = "…";
        let ell_w = self.measure_text(ellipsis, px_size, bold);
        let mut out = String::new();
        let mut w = 0.0;
        for ch in text.chars() {
            let cw = self
                .get_glyph(ch, px_size, bold)
                .map(|g| g.advance_px)
                .unwrap_or(px_size * 0.56);
            if w + cw + ell_w > max_w {
                break;
            }
            w += cw;
            out.push(ch);
        }
        out.push_str(ellipsis);
        out
    }

    fn wrap_lines(&mut self, text: &str, px_size: f32, bold: bool, max_w: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut current = String::new();
        for word in text.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if self.measure_text(&candidate, px_size, bold) <= max_w || current.is_empty() {
                current = candidate;
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }
}

// ============================================================================
// Software 2D Anti-Aliased Canvas for Glassmorphism UI
// ============================================================================

struct Canvas {
    width: usize,
    height: usize,
    scale: f32,
    pixels: Vec<u32>,
}

impl Canvas {
    fn new(width: usize, height: usize, scale: f32) -> Self {
        let w = width.max(1);
        let h = height.max(1);
        Self {
            width: w,
            height: h,
            scale: scale.max(0.5),
            pixels: vec![0xFF16_171B; w * h],
        }
    }

    fn clear_ambient_backdrop(&mut self) {
        let w = self.width as f32;
        let h = self.height as f32;
        let top_left = ColorRgba::rgb(0.078, 0.086, 0.112);
        let bottom_right = ColorRgba::rgb(0.062, 0.068, 0.088);
        let glow_center_x = w * 0.66;
        let glow_center_y = h * 0.42;
        let glow_radius = (w.max(h) * 0.48).max(1.0);

        for y in 0..self.height {
            let ty = y as f32 / h.max(1.0);
            let row = &mut self.pixels[y * self.width..(y + 1) * self.width];
            for (x, pixel) in row.iter_mut().enumerate() {
                let tx = x as f32 / w.max(1.0);
                let mut c = top_left.lerp(bottom_right, (tx * 0.45 + ty * 0.55).clamp(0.0, 1.0));
                let dx = x as f32 - glow_center_x;
                let dy = y as f32 - glow_center_y;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist < glow_radius {
                    let g = 1.0 - dist / glow_radius;
                    let g = g * g * 0.075;
                    c.r += 0.20 * g;
                    c.g += 0.48 * g;
                    c.b += 0.96 * g;
                }
                *pixel = c.to_xrgb();
            }
        }
    }

    fn blend_at(&mut self, px: i32, py: i32, color: ColorRgba) {
        if px < 0 || py < 0 || px >= self.width as i32 || py >= self.height as i32 {
            return;
        }
        let idx = (py as usize) * self.width + (px as usize);
        self.pixels[idx] = ColorRgba::blend_pixel(self.pixels[idx], color);
    }

    fn rounded_rect_sdf(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
        let qx = (px - cx).abs() - (hw - r);
        let qy = (py - cy).abs() - (hh - r);
        let out_x = qx.max(0.0);
        let out_y = qy.max(0.0);
        (out_x * out_x + out_y * out_y).sqrt() + qx.max(qy).min(0.0) - r
    }

    fn fill_rounded_rect_gradient(
        &mut self,
        rect: Rect,
        radius: f32,
        top_left: ColorRgba,
        bottom_right: ColorRgba,
    ) {
        let s = self.scale;
        let x0 = rect.x * s;
        let y0 = rect.y * s;
        let w = rect.w * s;
        let h = rect.h * s;
        if w <= 0.5 || h <= 0.5 {
            return;
        }
        let r = (radius * s).min(w * 0.5).min(h * 0.5);
        let cx = x0 + w * 0.5;
        let cy = y0 + h * 0.5;
        let hw = w * 0.5;
        let hh = h * 0.5;

        let min_x = (x0.floor() as i32 - 1).max(0);
        let max_x = ((x0 + w).ceil() as i32 + 1).min(self.width as i32 - 1);
        let min_y = (y0.floor() as i32 - 1).max(0);
        let max_y = ((y0 + h).ceil() as i32 + 1).min(self.height as i32 - 1);

        for py in min_y..=max_y {
            let fy = py as f32 + 0.5;
            let ty = ((fy - y0) / h).clamp(0.0, 1.0);
            for px in min_x..=max_x {
                let fx = px as f32 + 0.5;
                let d = Self::rounded_rect_sdf(fx, fy, cx, cy, hw, hh, r);
                if d < 0.75 {
                    let aa = (0.75 - d).clamp(0.0, 1.0);
                    let tx = ((fx - x0) / w).clamp(0.0, 1.0);
                    let col = top_left.lerp(bottom_right, (tx + ty) * 0.5).with_alpha(aa);
                    self.blend_at(px, py, col);
                }
            }
        }
    }

    fn fill_rounded_rect(&mut self, rect: Rect, radius: f32, color: ColorRgba) {
        self.fill_rounded_rect_gradient(rect, radius, color, color);
    }

    fn stroke_rounded_rect_gradient(
        &mut self,
        rect: Rect,
        radius: f32,
        stroke_w: f32,
        top_left: ColorRgba,
        bottom_right: ColorRgba,
    ) {
        let s = self.scale;
        let x0 = rect.x * s;
        let y0 = rect.y * s;
        let w = rect.w * s;
        let h = rect.h * s;
        if w <= 0.5 || h <= 0.5 {
            return;
        }
        let r = (radius * s).min(w * 0.5).min(h * 0.5);
        let half_sw = (stroke_w * s).max(1.0) * 0.5;
        let cx = x0 + w * 0.5;
        let cy = y0 + h * 0.5;
        let hw = w * 0.5 - half_sw;
        let hh = h * 0.5 - half_sw;

        let min_x = (x0.floor() as i32 - 2).max(0);
        let max_x = ((x0 + w).ceil() as i32 + 2).min(self.width as i32 - 1);
        let min_y = (y0.floor() as i32 - 2).max(0);
        let max_y = ((y0 + h).ceil() as i32 + 2).min(self.height as i32 - 1);

        for py in min_y..=max_y {
            let fy = py as f32 + 0.5;
            let ty = ((fy - y0) / h).clamp(0.0, 1.0);
            for px in min_x..=max_x {
                let fx = px as f32 + 0.5;
                let d = Self::rounded_rect_sdf(fx, fy, cx, cy, hw, hh, (r - half_sw).max(0.0));
                let edge_dist = d.abs() - half_sw;
                if edge_dist < 0.75 {
                    let aa = (0.75 - edge_dist).clamp(0.0, 1.0);
                    let tx = ((fx - x0) / w).clamp(0.0, 1.0);
                    let col = top_left.lerp(bottom_right, (tx + ty) * 0.5).with_alpha(aa);
                    self.blend_at(px, py, col);
                }
            }
        }
    }

    fn stroke_rounded_rect(&mut self, rect: Rect, radius: f32, stroke_w: f32, color: ColorRgba) {
        self.stroke_rounded_rect_gradient(rect, radius, stroke_w, color, color);
    }

    fn draw_shadow_rounded_rect(&mut self, rect: Rect, radius: f32, spread: f32, opacity: f32) {
        let s = self.scale;
        let pad = spread * s;
        let x0 = rect.x * s;
        let y0 = (rect.y + 6.0) * s;
        let w = rect.w * s;
        let h = rect.h * s;
        let r = (radius * s).min(w * 0.5).min(h * 0.5);
        let cx = x0 + w * 0.5;
        let cy = y0 + h * 0.5;
        let hw = w * 0.5;
        let hh = h * 0.5;

        let min_x = ((x0 - pad).floor() as i32).max(0);
        let max_x = ((x0 + w + pad).ceil() as i32).min(self.width as i32 - 1);
        let min_y = ((y0 - pad).floor() as i32).max(0);
        let max_y = ((y0 + h + pad).ceil() as i32).min(self.height as i32 - 1);

        for py in min_y..=max_y {
            let fy = py as f32 + 0.5;
            for px in min_x..=max_x {
                let fx = px as f32 + 0.5;
                let d = Self::rounded_rect_sdf(fx, fy, cx, cy, hw, hh, r);
                if d > -1.0 && d < pad {
                    let t = (1.0 - (d.max(0.0) / pad.max(1.0))).clamp(0.0, 1.0);
                    let alpha = t * t * opacity;
                    self.blend_at(px, py, ColorRgba::black(alpha));
                }
            }
        }
    }

    fn draw_glass_surface(&mut self, rect: Rect, radius: f32, tint_opacity: f32) {
        self.draw_shadow_rounded_rect(rect, radius, 16.0, 0.28);
        self.fill_rounded_rect(rect, radius, ColorRgba::rgba(0.115, 0.122, 0.145, 0.78));
        self.fill_rounded_rect_gradient(
            rect,
            radius,
            ColorRgba::white(tint_opacity + 0.06),
            ColorRgba::white(tint_opacity * 0.4),
        );
        self.stroke_rounded_rect_gradient(
            rect,
            radius,
            1.0,
            ColorRgba::white(0.36),
            ColorRgba::white(0.14),
        );
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, color: ColorRgba) {
        let s = self.scale;
        let pcx = cx * s;
        let pcy = cy * s;
        let pr = radius * s;
        let min_x = ((pcx - pr).floor() as i32 - 1).max(0);
        let max_x = ((pcx + pr).ceil() as i32 + 1).min(self.width as i32 - 1);
        let min_y = ((pcy - pr).floor() as i32 - 1).max(0);
        let max_y = ((pcy + pr).ceil() as i32 + 1).min(self.height as i32 - 1);

        for py in min_y..=max_y {
            let dy = (py as f32 + 0.5) - pcy;
            for px in min_x..=max_x {
                let dx = (px as f32 + 0.5) - pcx;
                let d = (dx * dx + dy * dy).sqrt() - pr;
                if d < 0.75 {
                    let aa = (0.75 - d).clamp(0.0, 1.0);
                    self.blend_at(px, py, color.with_alpha(aa));
                }
            }
        }
    }

    fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, stroke_w: f32, color: ColorRgba) {
        let s = self.scale;
        let pcx = cx * s;
        let pcy = cy * s;
        let pr = radius * s;
        let half_sw = (stroke_w * s).max(1.0) * 0.5;
        let outer = pr + half_sw + 1.0;
        let min_x = ((pcx - outer).floor() as i32).max(0);
        let max_x = ((pcx + outer).ceil() as i32).min(self.width as i32 - 1);
        let min_y = ((pcy - outer).floor() as i32).max(0);
        let max_y = ((pcy + outer).ceil() as i32).min(self.height as i32 - 1);

        for py in min_y..=max_y {
            let dy = (py as f32 + 0.5) - pcy;
            for px in min_x..=max_x {
                let dx = (px as f32 + 0.5) - pcx;
                let d = ((dx * dx + dy * dy).sqrt() - pr).abs() - half_sw;
                if d < 0.75 {
                    let aa = (0.75 - d).clamp(0.0, 1.0);
                    self.blend_at(px, py, color.with_alpha(aa));
                }
            }
        }
    }

    fn draw_radial_glow(&mut self, cx: f32, cy: f32, inner_r: f32, outer_r: f32, color: ColorRgba) {
        let s = self.scale;
        let pcx = cx * s;
        let pcy = cy * s;
        let ir = inner_r * s;
        let or = outer_r * s;
        let min_x = ((pcx - or).floor() as i32).max(0);
        let max_x = ((pcx + or).ceil() as i32).min(self.width as i32 - 1);
        let min_y = ((pcy - or).floor() as i32).max(0);
        let max_y = ((pcy + or).ceil() as i32).min(self.height as i32 - 1);

        for py in min_y..=max_y {
            let dy = (py as f32 + 0.5) - pcy;
            for px in min_x..=max_x {
                let dx = (px as f32 + 0.5) - pcx;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist < or {
                    let t = if dist <= ir {
                        1.0
                    } else {
                        1.0 - ((dist - ir) / (or - ir).max(1.0))
                    };
                    self.blend_at(px, py, color.with_alpha(t * t));
                }
            }
        }
    }

    fn draw_line_aa(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        stroke_w: f32,
        color: ColorRgba,
    ) {
        let s = self.scale;
        let ax = x0 * s;
        let ay = y0 * s;
        let bx = x1 * s;
        let by = y1 * s;
        let half_sw = (stroke_w * s).max(1.0) * 0.5;
        let pad = half_sw + 1.5;
        let min_x = (ax.min(bx) - pad).floor() as i32;
        let max_x = (ax.max(bx) + pad).ceil() as i32;
        let min_y = (ay.min(by) - pad).floor() as i32;
        let max_y = (ay.max(by) + pad).ceil() as i32;

        let abx = bx - ax;
        let aby = by - ay;
        let len_sq = abx * abx + aby * aby;

        for py in min_y..=max_y {
            let fy = py as f32 + 0.5;
            for px in min_x..=max_x {
                let fx = px as f32 + 0.5;
                let t = if len_sq <= 1e-6 {
                    0.0
                } else {
                    (((fx - ax) * abx + (fy - ay) * aby) / len_sq).clamp(0.0, 1.0)
                };
                let dx = fx - (ax + abx * t);
                let dy = fy - (ay + aby * t);
                let d = (dx * dx + dy * dy).sqrt() - half_sw;
                if d < 0.75 {
                    let aa = (0.75 - d).clamp(0.0, 1.0);
                    self.blend_at(px, py, color.with_alpha(aa));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        fonts: &mut FontRenderer,
        text: &str,
        x: f32,
        y_top: f32,
        size: f32,
        bold: bool,
        color: ColorRgba,
    ) {
        let px_size = size * self.scale;
        let baseline_y = (y_top + size * 0.92) * self.scale;
        let mut cursor_x = x * self.scale;

        for ch in text.chars() {
            if let Some(glyph) = fonts.get_glyph(ch, px_size, bold) {
                let gx0 = cursor_x.round() as i32 + glyph.offset_x;
                let gy0 = baseline_y.round() as i32 + glyph.offset_y;
                for row in 0..glyph.height {
                    let py = gy0 + row as i32;
                    if py < 0 || py >= self.height as i32 {
                        continue;
                    }
                    let row_offset = row * glyph.width;
                    for col in 0..glyph.width {
                        let alpha = glyph.bitmap[row_offset + col];
                        if alpha == 0 {
                            continue;
                        }
                        let px = gx0 + col as i32;
                        if px < 0 || px >= self.width as i32 {
                            continue;
                        }
                        let idx = (py as usize) * self.width + (px as usize);
                        self.pixels[idx] = ColorRgba::blend_pixel(
                            self.pixels[idx],
                            color.with_alpha(alpha as f32 / 255.0),
                        );
                    }
                }
                cursor_x += glyph.advance_px;
            } else {
                cursor_x += px_size * 0.56;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text_centered(
        &mut self,
        fonts: &mut FontRenderer,
        text: &str,
        center_x: f32,
        y_top: f32,
        size: f32,
        bold: bool,
        color: ColorRgba,
    ) {
        let w = fonts.measure_text(text, size, bold);
        self.draw_text(fonts, text, center_x - w * 0.5, y_top, size, bold, color);
    }

    // ---- Crisp Vector Icons ------------------------------------------------

    fn draw_icon_iphone(&mut self, cx: f32, cy: f32, h: f32, stroke: f32, color: ColorRgba) {
        let w = h * 0.52;
        let r = w * 0.22;
        let rect = Rect::new(cx - w * 0.5, cy - h * 0.5, w, h);
        self.stroke_rounded_rect(rect, r, stroke, color);
        // Dynamic Island / notch pill
        let island_w = w * 0.34;
        let island_h = (h * 0.06).max(2.0);
        self.fill_rounded_rect(
            Rect::new(cx - island_w * 0.5, rect.y + h * 0.09, island_w, island_h),
            island_h * 0.5,
            color,
        );
        // Home indicator bar
        let bar_w = w * 0.36;
        self.draw_line_aa(
            cx - bar_w * 0.5,
            rect.y + h * 0.89,
            cx + bar_w * 0.5,
            rect.y + h * 0.89,
            (stroke * 0.85).max(1.0),
            color.with_alpha(0.8),
        );
    }

    fn draw_icon_refresh(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        let steps = 20;
        let start = -0.3_f32;
        let end = 4.6_f32;
        for i in 0..steps {
            let t0 = start + (end - start) * (i as f32 / steps as f32);
            let t1 = start + (end - start) * ((i + 1) as f32 / steps as f32);
            self.draw_line_aa(
                cx + r * t0.cos(),
                cy + r * t0.sin(),
                cx + r * t1.cos(),
                cy + r * t1.sin(),
                1.5,
                color,
            );
        }
        let tip_x = cx + r * start.cos();
        let tip_y = cy + r * start.sin();
        self.draw_line_aa(tip_x, tip_y, tip_x - 3.2, tip_y - 0.8, 1.5, color);
        self.draw_line_aa(tip_x, tip_y, tip_x + 0.8, tip_y - 3.2, 1.5, color);
    }

    fn draw_icon_gear(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.stroke_circle(cx, cy, r * 0.58, 1.5, color);
        self.stroke_circle(cx, cy, r * 0.24, 1.3, color);
        for i in 0..8 {
            let a = (i as f32) * std::f32::consts::FRAC_PI_4;
            self.draw_line_aa(
                cx + r * 0.68 * a.cos(),
                cy + r * 0.68 * a.sin(),
                cx + r * 0.96 * a.cos(),
                cy + r * 0.96 * a.sin(),
                1.8,
                color,
            );
        }
    }

    fn draw_icon_sparkles(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.draw_line_aa(cx, cy - r, cx, cy + r, 1.4, color);
        self.draw_line_aa(cx - r, cy, cx + r, cy, 1.4, color);
        let d = r * 0.45;
        self.draw_line_aa(cx - d, cy - d, cx + d, cy + d, 1.1, color.with_alpha(0.75));
        self.draw_line_aa(cx - d, cy + d, cx + d, cy - d, 1.1, color.with_alpha(0.75));
    }

    fn draw_icon_usb(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.draw_line_aa(cx - r, cy, cx + r * 0.65, cy, 1.5, color);
        self.fill_circle(cx - r * 0.9, cy, 1.6, color);
        self.draw_line_aa(cx + r * 0.5, cy - 2.6, cx + r, cy, 1.5, color);
        self.draw_line_aa(cx + r * 0.5, cy + 2.6, cx + r, cy, 1.5, color);
        self.draw_line_aa(cx - r * 0.3, cy, cx + r * 0.15, cy - r * 0.58, 1.2, color);
        self.fill_circle(cx + r * 0.22, cy - r * 0.58, 1.2, color);
    }

    fn draw_icon_wifi(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.fill_circle(cx, cy + r * 0.55, (r * 0.15).max(1.2), color);
        for (rad, alpha) in [(r * 0.48, 0.95), (r * 0.88, 0.85)] {
            let steps = 10;
            let a0 = -2.35_f32;
            let a1 = -0.79_f32;
            for i in 0..steps {
                let t0 = a0 + (a1 - a0) * (i as f32 / steps as f32);
                let t1 = a0 + (a1 - a0) * ((i + 1) as f32 / steps as f32);
                self.draw_line_aa(
                    cx + rad * t0.cos(),
                    cy + r * 0.5 + rad * t0.sin(),
                    cx + rad * t1.cos(),
                    cy + r * 0.5 + rad * t1.sin(),
                    1.4,
                    color.with_alpha(alpha),
                );
            }
        }
    }

    fn draw_icon_airplay(&mut self, cx: f32, cy: f32, size: f32, stroke: f32, color: ColorRgba) {
        let w = size;
        let h = size * 0.72;
        let rect = Rect::new(cx - w * 0.5, cy - h * 0.56, w, h);
        self.stroke_rounded_rect(rect, size * 0.12, stroke, color);
        // Bottom triangle overlapping screen base
        let tri_h = size * 0.26;
        let tri_w = size * 0.32;
        let base_y = rect.y + rect.h + tri_h * 0.48;
        let apex_y = rect.y + rect.h - tri_h * 0.45;
        self.draw_line_aa(cx, apex_y, cx - tri_w * 0.5, base_y, stroke, color);
        self.draw_line_aa(cx, apex_y, cx + tri_w * 0.5, base_y, stroke, color);
        self.draw_line_aa(
            cx - tri_w * 0.5,
            base_y,
            cx + tri_w * 0.5,
            base_y,
            stroke,
            color,
        );
    }

    fn draw_icon_play(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        let s = self.scale;
        let x0 = (cx - r * 0.45) * s;
        let x1 = (cx + r * 0.7) * s;
        let y0 = (cy - r * 0.65) * s;
        let y1 = (cy + r * 0.65) * s;
        let yc = cy * s;
        let min_x = x0.floor() as i32;
        let max_x = x1.ceil() as i32;
        let min_y = y0.floor() as i32;
        let max_y = y1.ceil() as i32;
        for py in min_y..=max_y {
            let fy = py as f32 + 0.5;
            for px in min_x..=max_x {
                let fx = px as f32 + 0.5;
                if fx >= x0 && fx <= x1 {
                    let t = ((fx - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0);
                    let half_h = (yc - y0) * (1.0 - t);
                    if (fy - yc).abs() <= half_h {
                        self.blend_at(px, py, color);
                    }
                }
            }
        }
    }

    fn draw_icon_stop(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.fill_rounded_rect(
            Rect::new(cx - r * 0.55, cy - r * 0.55, r * 1.1, r * 1.1),
            2.0,
            color,
        );
    }

    fn draw_icon_close(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.draw_line_aa(cx - r, cy - r, cx + r, cy + r, 1.5, color);
        self.draw_line_aa(cx - r, cy + r, cx + r, cy - r, 1.5, color);
    }

    fn draw_icon_check(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.draw_line_aa(
            cx - r * 0.65,
            cy + r * 0.05,
            cx - r * 0.1,
            cy + r * 0.55,
            1.8,
            color,
        );
        self.draw_line_aa(
            cx - r * 0.1,
            cy + r * 0.55,
            cx + r * 0.7,
            cy - r * 0.45,
            1.8,
            color,
        );
    }

    fn draw_icon_bell(&mut self, cx: f32, cy: f32, r: f32, color: ColorRgba) {
        self.fill_circle(cx, cy - r * 0.10, r * 0.55, color);
        self.draw_line_aa(
            cx - r * 0.72,
            cy + r * 0.48,
            cx + r * 0.72,
            cy + r * 0.48,
            1.5,
            color,
        );
        self.fill_circle(cx, cy + r * 0.72, r * 0.22, color);
    }
}

// ============================================================================
// Launcher Application State & Rendering
// ============================================================================

struct LauncherApp {
    window: Option<Window>,
    renderer: Option<MetalRenderer>,
    fonts: FontRenderer,
    proxy: EventLoopProxy<LauncherEvent>,
    settings: LauncherSettings,
    devices: Vec<LauncherDevice>,
    nearby_devices: Vec<NearbyDevice>,
    selected_id: Option<String>,
    device_scroll: f32,
    is_scanning: bool,
    is_manual_scanning: bool,
    error: Option<String>,
    status_notice: Option<String>,
    available_update: Option<AvailableUpdate>,
    update_dismissed: bool,
    is_checking_updates: bool,
    update_status_message: Option<String>,
    backend_running: bool,
    backend_cancel: Option<tokio::sync::oneshot::Sender<()>>,
    did_auto_connect: bool,
    pairing_in_progress: bool,
    show_settings: bool,
    show_notifications_panel: bool,
    bluez_status: String,
    desktop_notifier: LinuxDesktopNotifier,
    notification_engine: Option<NotificationEngine>,
    notification_history: Vec<IosNotification>,
    hovered_action: Option<HitAction>,
    hit_regions: Vec<HitRegion>,
    modifiers: ModifiersState,
    cursor_logical: Option<(f32, f32)>,
}

impl LauncherApp {
    fn new(proxy: EventLoopProxy<LauncherEvent>) -> Self {
        let settings = load_launcher_settings();
        let notification_engine = NotificationEngine::new(settings.notifications.clone()).ok();
        Self {
            window: None,
            renderer: None,
            fonts: FontRenderer::new(),
            proxy,
            settings,
            devices: Vec::new(),
            nearby_devices: Vec::new(),
            selected_id: None,
            device_scroll: 0.0,
            is_scanning: false,
            is_manual_scanning: false,
            error: None,
            status_notice: None,
            available_update: None,
            update_dismissed: false,
            is_checking_updates: false,
            update_status_message: None,
            backend_running: false,
            backend_cancel: None,
            did_auto_connect: false,
            pairing_in_progress: false,
            show_settings: false,
            show_notifications_panel: false,
            bluez_status: "Checking BlueZ ANCS status…".to_string(),
            desktop_notifier: LinuxDesktopNotifier::new(),
            notification_engine,
            notification_history: Vec::new(),
            hovered_action: None,
            hit_regions: Vec::new(),
            modifiers: ModifiersState::default(),
            cursor_logical: None,
        }
    }

    fn refresh_bluez_status(&self) {
        let proxy = self.proxy.clone();
        tokio::spawn(async move {
            let status = inspect_bluez_status();
            let _ = proxy.send_event(LauncherEvent::BluezStatusUpdated(status.detail));
        });
    }

    fn sync_notification_settings(&mut self) {
        save_launcher_settings(&self.settings);
        if let Some(engine) = &mut self.notification_engine {
            let _ = engine.handle_host_message(
                HostToDaemonMessage::UpdateSettings {
                    settings: self.settings.notifications.clone(),
                },
                std::time::Instant::now(),
            );
        }
    }

    fn send_test_notification(&mut self) {
        if let Some(engine) = &mut self.notification_engine {
            let msgs = engine.handle_host_message(
                HostToDaemonMessage::SendTestNotification,
                std::time::Instant::now(),
            );
            for msg in msgs {
                if let DaemonToHostMessage::DeliverNotification { notification } = msg {
                    self.desktop_notifier.deliver(&notification);
                    self.notification_history.insert(0, notification);
                    if self.notification_history.len() > 100 {
                        self.notification_history.truncate(100);
                    }
                }
            }
        }
    }

    fn app_version_label() -> String {
        format!("v{}", env!("CARGO_PKG_VERSION"))
    }

    fn filtered_devices(&self) -> Vec<LauncherDevice> {
        filter_devices_for_transport(&self.devices, self.settings.transport)
    }

    fn selected_device(&self) -> Option<LauncherDevice> {
        let filtered = self.filtered_devices();
        if let Some(id) = &self.selected_id {
            if let Some(found) = filtered.iter().find(|d| &d.id() == id) {
                return Some(found.clone());
            }
        }
        filtered.into_iter().next()
    }

    fn sync_selection(&mut self) {
        let filtered = self.filtered_devices();
        let valid = self
            .selected_id
            .as_ref()
            .is_some_and(|id| filtered.iter().any(|d| &d.id() == id));
        if !valid {
            self.selected_id = filtered.first().map(LauncherDevice::id);
        }
    }

    fn trigger_scan(&mut self, manual: bool) {
        if self.is_scanning {
            return;
        }
        self.is_scanning = true;
        if manual {
            self.is_manual_scanning = true;
            self.request_redraw();
        }
        let proxy = self.proxy.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let devices_res = scan_devices_json().await;
            if manual {
                let elapsed = started.elapsed();
                let min_visual = std::time::Duration::from_millis(550);
                if elapsed < min_visual {
                    tokio::time::sleep(min_visual - elapsed).await;
                }
            }
            let _ = proxy.send_event(LauncherEvent::DevicesScanned {
                manual,
                result: devices_res,
            });
            let nearby = scan_nearby_avahi().await;
            let _ = proxy.send_event(LauncherEvent::NearbyScanned(nearby));
        });
    }

    fn trigger_update_check(&mut self, silent: bool) {
        if self.is_checking_updates {
            return;
        }
        self.is_checking_updates = true;
        if !silent {
            self.update_status_message = Some("Checking GitHub releases…".to_string());
        }
        self.request_redraw();
        let proxy = self.proxy.clone();
        tokio::spawn(async move {
            let (result, status) = check_github_updates().await;
            let _ = proxy.send_event(LauncherEvent::UpdateChecked {
                silent,
                result,
                status,
            });
        });
    }

    fn start_pair_wifi(&mut self, udid: String) {
        if self.pairing_in_progress {
            return;
        }
        self.pairing_in_progress = true;
        self.error = None;
        self.status_notice = Some("Pairing Wi-Fi… Accept the prompt on your iPhone.".to_string());
        self.request_redraw();
        let proxy = self.proxy.clone();
        tokio::spawn(async move {
            let res = run_pair_subcommand(&udid).await;
            let _ = proxy.send_event(LauncherEvent::PairFinished(res));
        });
    }

    fn start_mirror_session(&mut self) {
        if self.backend_running {
            return;
        }
        let selected = self.selected_device();
        if self.settings.transport != TransportPreference::Airplay && selected.is_none() {
            return;
        }
        let Ok(exe) = env::current_exe() else {
            self.error = Some("Could not locate the iphone-mirror executable.".to_string());
            self.request_redraw();
            return;
        };

        let mut cmd = tokio::process::Command::new(exe);
        cmd.arg("mirror")
            .env("IPHONE_MIRROR_SESSION_WORKER", "1")
            .env(
                "IPHONE_MIRROR_AUTO_ROTATE",
                if self.settings.auto_rotate { "1" } else { "0" },
            )
            .kill_on_drop(true);

        if self.settings.transport == TransportPreference::Airplay {
            cmd.env("IPHONE_MIRROR_TRANSPORT", "airplay");
            if let Some(dev) = &selected {
                cmd.env("IPHONE_MIRROR_UDID", &dev.udid);
                if dev.is_remote() {
                    if let (Some(host), Some(port)) = (&dev.host, dev.port) {
                        cmd.env("IPHONE_MIRROR_WIFI_HOST", host);
                        cmd.env("IPHONE_MIRROR_WIFI_PORT", port.to_string());
                        cmd.env(
                            "IPHONE_MIRROR_WIFI_NAME",
                            dev.name.as_deref().unwrap_or(&dev.udid),
                        );
                        cmd.env("IPHONE_MIRROR_WIFI_IDENTIFIER", &dev.udid);
                    }
                }
            }
        } else if let Some(dev) = &selected {
            cmd.env("IPHONE_MIRROR_UDID", &dev.udid);
            cmd.env(
                "IPHONE_MIRROR_TRANSPORT",
                if dev.is_network() { "wifi" } else { "usb" },
            );
            if dev.is_remote() {
                if let (Some(host), Some(port)) = (&dev.host, dev.port) {
                    cmd.env("IPHONE_MIRROR_WIFI_HOST", host);
                    cmd.env("IPHONE_MIRROR_WIFI_PORT", port.to_string());
                    cmd.env(
                        "IPHONE_MIRROR_WIFI_NAME",
                        dev.name.as_deref().unwrap_or(&dev.udid),
                    );
                    cmd.env("IPHONE_MIRROR_WIFI_IDENTIFIER", &dev.udid);
                }
            } else {
                cmd.env_remove("IPHONE_MIRROR_WIFI_HOST");
                cmd.env_remove("IPHONE_MIRROR_WIFI_PORT");
                cmd.env_remove("IPHONE_MIRROR_WIFI_NAME");
                cmd.env_remove("IPHONE_MIRROR_WIFI_IDENTIFIER");
            }
        }

        let log_path = env::temp_dir().join("iphone-mirror-backend.log");
        if let Ok(log_file) = fs::File::create(&log_path) {
            if let Ok(err_file) = log_file.try_clone() {
                cmd.stdout(Stdio::from(log_file));
                cmd.stderr(Stdio::from(err_file));
            }
        }

        match cmd.spawn() {
            Ok(mut child) => {
                let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
                self.backend_cancel = Some(cancel_tx);
                self.backend_running = true;
                self.error = None;
                self.status_notice =
                    Some("Mirror session active — waiting for iPhone video stream…".to_string());
                let proxy = self.proxy.clone();
                tokio::spawn(async move {
                    let exit_ok = tokio::select! {
                        status = child.wait() => status.map(|s| s.success()).unwrap_or(false),
                        _ = &mut cancel_rx => {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            true
                        }
                    };
                    let error_detail = if exit_ok {
                        None
                    } else {
                        fs::read_to_string(&log_path)
                            .ok()
                            .map(|text| {
                                text.lines()
                                    .rev()
                                    .take(2)
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .filter(|s| !s.trim().is_empty())
                    };
                    let _ = proxy.send_event(LauncherEvent::BackendExited {
                        exit_ok,
                        error_detail,
                    });
                });
                self.request_redraw();
            }
            Err(err) => {
                self.error = Some(format!("Couldn't start the mirror session: {err}"));
                self.request_redraw();
            }
        }
    }

    fn stop_mirror_session(&mut self) {
        if let Some(cancel) = self.backend_cancel.take() {
            let _ = cancel.send(());
        }
    }

    fn open_update_url(&self) {
        if let Some(update) = &self.available_update {
            let _ = std::process::Command::new("xdg-open")
                .arg(&update.url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }

    fn handle_action(&mut self, action: HitAction) {
        match action {
            HitAction::Refresh => self.trigger_scan(true),
            HitAction::OpenSettings => {
                self.show_settings = true;
            }
            HitAction::CloseSettings => {
                self.show_settings = false;
            }
            HitAction::ToggleNotificationsPanel => {
                self.show_notifications_panel = !self.show_notifications_panel;
                if self.show_notifications_panel {
                    self.refresh_bluez_status();
                }
            }
            HitAction::ToggleNotificationsEnabled => {
                self.settings.notifications.enabled = !self.settings.notifications.enabled;
                self.sync_notification_settings();
            }
            HitAction::ToggleMuteWhileMirroring => {
                self.settings.notifications.mute_while_mirroring =
                    !self.settings.notifications.mute_while_mirroring;
                self.sync_notification_settings();
            }
            HitAction::ToggleHidePreviews => {
                self.settings.notifications.hide_previews =
                    !self.settings.notifications.hide_previews;
                self.sync_notification_settings();
            }
            HitAction::SendTestNotification => {
                self.send_test_notification();
            }
            HitAction::SolicitBluezAncs => {
                let proxy = self.proxy.clone();
                tokio::spawn(async move {
                    let _ = solicit_ancs_advertisement();
                    let status = inspect_bluez_status();
                    let _ = proxy.send_event(LauncherEvent::BluezStatusUpdated(status.detail));
                });
            }
            HitAction::ClearNotifications => {
                self.notification_history.clear();
            }
            HitAction::SelectMode(mode) => {
                self.settings.transport = mode;
                save_launcher_settings(&self.settings);
                self.sync_selection();
                if mode == TransportPreference::Airplay {
                    self.error = None;
                }
            }
            HitAction::SelectDevice(id) => {
                self.selected_id = Some(id);
            }
            HitAction::PrimaryAction => {
                if self.backend_running {
                    self.stop_mirror_session();
                } else {
                    self.start_mirror_session();
                }
            }
            HitAction::PairWifi => {
                if let Some(dev) = self.devices.iter().find(|d| d.is_usb()).cloned() {
                    self.start_pair_wifi(dev.udid);
                } else {
                    self.error = Some(
                        "Connect your iPhone via USB cable first, then click Pair Wi-Fi via USB."
                            .to_string(),
                    );
                }
            }
            HitAction::CheckOrOpenUpdate => {
                if self.available_update.is_some() {
                    self.open_update_url();
                } else {
                    self.trigger_update_check(false);
                }
            }
            HitAction::DownloadUpdate => {
                self.open_update_url();
            }
            HitAction::DismissUpdate => {
                self.update_dismissed = true;
            }
            HitAction::ToggleAutoConnect => {
                self.settings.auto_connect = !self.settings.auto_connect;
                save_launcher_settings(&self.settings);
            }
            HitAction::ToggleAutoRotate => {
                self.settings.auto_rotate = !self.settings.auto_rotate;
                save_launcher_settings(&self.settings);
            }
            HitAction::ToggleAutoCheckUpdates => {
                self.settings.auto_check_updates = !self.settings.auto_check_updates;
                save_launcher_settings(&self.settings);
            }
            HitAction::SettingsCheckUpdates => {
                self.trigger_update_check(false);
            }
            HitAction::SettingsSelectTransport(mode) => {
                self.settings.transport = mode;
                save_launcher_settings(&self.settings);
                self.sync_selection();
            }
        }
        self.request_redraw();
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn render(&mut self) {
        let (phys, scale) = {
            let Some(window) = &self.window else { return };
            if self.renderer.is_none() {
                return;
            }
            (
                window.inner_size(),
                (window.scale_factor() as f32).clamp(0.75, 3.0),
            )
        };
        if phys.width == 0 || phys.height == 0 {
            return;
        }
        let lw = (phys.width as f32 / scale).max(320.0);
        let lh = (phys.height as f32 / scale).max(240.0);

        let mut canvas = Canvas::new(phys.width as usize, phys.height as usize, scale);
        canvas.clear_ambient_backdrop();
        self.hit_regions.clear();

        let sidebar_rect = Rect::new(
            OUTER_PAD,
            TOP_PAD,
            SIDEBAR_WIDTH,
            (lh - TOP_PAD - OUTER_PAD).max(200.0),
        );
        let stage_x = sidebar_rect.x + sidebar_rect.w + PANEL_GAP;
        let stage_rect = Rect::new(
            stage_x,
            TOP_PAD,
            (lw - stage_x - OUTER_PAD).max(260.0),
            sidebar_rect.h,
        );

        self.draw_sidebar(&mut canvas, sidebar_rect);
        if self.show_notifications_panel {
            self.draw_notifications_stage(&mut canvas, stage_rect);
        } else {
            self.draw_stage(&mut canvas, stage_rect);
        }

        if self.show_settings {
            self.draw_settings_modal(&mut canvas, lw, lh);
        }

        if let Some(renderer) = &self.renderer {
            if let Err(err) = renderer.present_xrgb(&canvas.pixels, phys.width, phys.height) {
                tracing::warn!(%err, "Failed to present launcher frame");
            }
        }
    }

    fn draw_sidebar(&mut self, c: &mut Canvas, panel: Rect) {
        c.draw_glass_surface(panel, 20.0, 0.06);
        let pad = 18.0;
        let ix = panel.x + pad;
        let iw = panel.w - pad * 2.0;
        let mut y = panel.y + pad;

        // Header
        c.draw_icon_iphone(ix + 7.0, y + 12.0, 15.0, 1.4, ColorRgba::white(0.95));
        c.draw_text(
            &mut self.fonts,
            "iPhone Mirror",
            ix + 21.0,
            y + 4.0,
            14.0,
            true,
            ColorRgba::white(0.98),
        );

        let refresh_rect = Rect::new(ix + iw - 24.0, y, 24.0, 24.0);
        let settings_rect = Rect::new(ix + iw - 54.0, y, 24.0, 24.0);
        let bell_rect = Rect::new(ix + iw - 84.0, y, 24.0, 24.0);

        let bell_hover = self.hovered_action == Some(HitAction::ToggleNotificationsPanel);
        c.fill_circle(
            bell_rect.x + 12.0,
            bell_rect.y + 12.0,
            12.0,
            if self.show_notifications_panel {
                ColorRgba::rgba(0.28, 0.62, 1.0, 0.45)
            } else {
                ColorRgba::white(if bell_hover { 0.16 } else { 0.08 })
            },
        );
        c.draw_icon_bell(
            bell_rect.x + 12.0,
            bell_rect.y + 12.0,
            5.5,
            ColorRgba::white(if self.settings.notifications.enabled {
                0.90
            } else {
                0.45
            }),
        );
        self.hit_regions.push(HitRegion {
            rect: bell_rect,
            action: HitAction::ToggleNotificationsPanel,
        });

        let settings_hover = self.hovered_action == Some(HitAction::OpenSettings);
        c.fill_circle(
            settings_rect.x + 12.0,
            settings_rect.y + 12.0,
            12.0,
            ColorRgba::white(if settings_hover { 0.16 } else { 0.08 }),
        );
        c.draw_icon_gear(
            settings_rect.x + 12.0,
            settings_rect.y + 12.0,
            5.5,
            ColorRgba::white(0.84),
        );
        self.hit_regions.push(HitRegion {
            rect: settings_rect,
            action: HitAction::OpenSettings,
        });

        let refresh_hover = self.hovered_action == Some(HitAction::Refresh);
        c.fill_circle(
            refresh_rect.x + 12.0,
            refresh_rect.y + 12.0,
            12.0,
            if self.is_manual_scanning {
                ColorRgba::rgba(0.28, 0.62, 1.0, 0.45)
            } else {
                ColorRgba::white(if refresh_hover { 0.16 } else { 0.08 })
            },
        );
        c.draw_icon_refresh(
            refresh_rect.x + 12.0,
            refresh_rect.y + 12.0,
            5.0,
            if self.is_manual_scanning {
                ColorRgba::rgb(0.65, 0.88, 1.0)
            } else {
                ColorRgba::white(0.85)
            },
        );
        self.hit_regions.push(HitRegion {
            rect: refresh_rect,
            action: HitAction::Refresh,
        });

        y += 38.0;

        // 4-Way GlassModeSelector
        let selector_rect = Rect::new(ix, y, iw, 46.0);
        c.fill_rounded_rect(selector_rect, 12.0, ColorRgba::black(0.25));
        c.stroke_rounded_rect(selector_rect, 12.0, 1.0, ColorRgba::white(0.12));

        let modes = [
            (TransportPreference::Auto, "Auto"),
            (TransportPreference::Usb, "USB"),
            (TransportPreference::Wifi, "Wi-Fi"),
            (TransportPreference::Airplay, "AirPlay"),
        ];
        let seg_gap = 4.0;
        let inner_seg_w = (iw - 8.0 - seg_gap * 3.0) / 4.0;
        for (idx, (mode, label)) in modes.iter().enumerate() {
            let sx = ix + 4.0 + idx as f32 * (inner_seg_w + seg_gap);
            let seg = Rect::new(sx, y + 4.0, inner_seg_w, 38.0);
            let is_selected = self.settings.transport == *mode;
            let action = HitAction::SelectMode(*mode);
            let is_hover = self.hovered_action.as_ref() == Some(&action);
            if is_selected {
                c.fill_rounded_rect_gradient(
                    seg,
                    9.0,
                    ColorRgba::white(0.24),
                    ColorRgba::white(0.10),
                );
                c.stroke_rounded_rect(seg, 9.0, 1.0, ColorRgba::white(0.32));
            } else if is_hover {
                c.fill_rounded_rect(seg, 9.0, ColorRgba::white(0.08));
            }
            let icon_col = ColorRgba::white(if is_selected { 0.98 } else { 0.65 });
            let icx = seg.x + seg.w * 0.5;
            let icy = seg.y + 12.0;
            match mode {
                TransportPreference::Auto => c.draw_icon_sparkles(icx, icy, 4.8, icon_col),
                TransportPreference::Usb => c.draw_icon_usb(icx, icy, 5.2, icon_col),
                TransportPreference::Wifi => c.draw_icon_wifi(icx, icy, 5.4, icon_col),
                TransportPreference::Airplay => c.draw_icon_airplay(icx, icy, 10.5, 1.3, icon_col),
            }
            c.draw_text_centered(
                &mut self.fonts,
                label,
                icx,
                seg.y + 21.0,
                10.2,
                is_selected,
                icon_col,
            );
            self.hit_regions.push(HitRegion { rect: seg, action });
        }

        y += 60.0;

        let selected = self.selected_device();
        if self.settings.transport == TransportPreference::Airplay {
            let card = Rect::new(ix, y, iw, 54.0);
            c.fill_rounded_rect(card, 11.0, ColorRgba::white(0.07));
            c.stroke_rounded_rect(card, 11.0, 1.0, ColorRgba::white(0.16));
            c.draw_icon_airplay(
                card.x + 19.0,
                card.y + 27.0,
                14.0,
                1.4,
                ColorRgba::rgb(0.55, 0.85, 1.0),
            );
            c.draw_text(
                &mut self.fonts,
                "AirPlay Receiver Mode",
                card.x + 36.0,
                card.y + 10.0,
                11.8,
                true,
                ColorRgba::white(0.96),
            );
            let sub = if let Some(dev) = &selected {
                format!("Hybrid HID ready via {}", dev.display_name())
            } else {
                "Standard H.264 wireless receiver".to_string()
            };
            let sub_trunc = self.fonts.truncate_to_width(&sub, 10.3, false, iw - 46.0);
            c.draw_text(
                &mut self.fonts,
                &sub_trunc,
                card.x + 36.0,
                card.y + 28.0,
                10.3,
                false,
                ColorRgba::white(0.62),
            );
            y += 66.0;
        }

        let filtered = self.filtered_devices();
        let mut seen_nearby = BTreeSet::new();
        let unique_nearby: Vec<NearbyDevice> = self
            .nearby_devices
            .iter()
            .filter(|n| seen_nearby.insert(n.display_name().to_ascii_lowercase()))
            .cloned()
            .collect();

        let footer_h = 40.0;
        let nearby_h = if filtered.is_empty()
            && self.settings.transport != TransportPreference::Usb
            && !unique_nearby.is_empty()
        {
            22.0 + (unique_nearby.len().min(3) as f32) * 32.0
        } else {
            0.0
        };
        let list_bottom = panel.y + panel.h - pad - footer_h - nearby_h - 8.0;

        if filtered.is_empty() {
            let icon_cy = y + 20.0;
            if self.settings.transport == TransportPreference::Airplay {
                c.draw_icon_wifi(ix + 12.0, icon_cy, 10.0, ColorRgba::white(0.65));
            } else {
                c.draw_icon_iphone(ix + 10.0, icon_cy, 20.0, 1.3, ColorRgba::white(0.65));
            }
            let title = if self.settings.transport == TransportPreference::Airplay {
                "No wired/paired iPhone"
            } else {
                "No iPhone found"
            };
            c.draw_text(
                &mut self.fonts,
                title,
                ix,
                y + 36.0,
                12.8,
                true,
                ColorRgba::white(0.95),
            );
            let hint = match self.settings.transport {
                TransportPreference::Airplay => {
                    "Start the receiver directly, then pick iPhone Mirror in Control Center."
                }
                TransportPreference::Wifi => {
                    "Connect once via cable and tap Pair Wi-Fi to mirror wirelessly."
                }
                _ => "Connect by USB, or pair once by cable for Wi-Fi mirroring.",
            };
            let lines = self.fonts.wrap_lines(hint, 11.2, false, iw);
            for (li, line) in lines.iter().enumerate() {
                c.draw_text(
                    &mut self.fonts,
                    line,
                    ix,
                    y + 56.0 + li as f32 * 15.0,
                    11.2,
                    false,
                    ColorRgba::white(0.62),
                );
            }
        } else {
            let row_h = 54.0;
            let row_gap = 6.0;
            let total_h = filtered.len() as f32 * (row_h + row_gap) - row_gap;
            let avail_h = (list_bottom - y).max(row_h);
            let max_scroll = (total_h - avail_h).max(0.0);
            self.device_scroll = self.device_scroll.clamp(0.0, max_scroll);

            let mut ry = y - self.device_scroll;
            for dev in &filtered {
                if ry + row_h >= y - 2.0 && ry <= list_bottom {
                    let row_rect = Rect::new(ix, ry, iw, row_h);
                    let is_sel = selected.as_ref().is_some_and(|s| s.id() == dev.id());
                    let action = HitAction::SelectDevice(dev.id());
                    let is_hover = self.hovered_action.as_ref() == Some(&action);
                    if is_sel {
                        c.fill_rounded_rect_gradient(
                            row_rect,
                            12.0,
                            ColorRgba::rgba(0.25, 0.55, 1.0, 0.85),
                            ColorRgba::rgba(0.18, 0.42, 0.92, 0.75),
                        );
                        c.stroke_rounded_rect(row_rect, 12.0, 1.0, ColorRgba::white(0.35));
                    } else {
                        c.fill_rounded_rect_gradient(
                            row_rect,
                            12.0,
                            ColorRgba::white(if is_hover { 0.11 } else { 0.06 }),
                            ColorRgba::white(if is_hover { 0.05 } else { 0.02 }),
                        );
                        c.stroke_rounded_rect(row_rect, 12.0, 1.0, ColorRgba::white(0.10));
                    }
                    c.fill_circle(
                        row_rect.x + 28.0,
                        row_rect.y + 27.0,
                        16.0,
                        ColorRgba::white(if is_sel { 0.22 } else { 0.08 }),
                    );
                    let badge_col = if is_sel {
                        ColorRgba::white(0.98)
                    } else {
                        ColorRgba::rgb(0.45, 0.80, 1.0)
                    };
                    if dev.is_usb() {
                        c.draw_icon_usb(row_rect.x + 28.0, row_rect.y + 27.0, 7.0, badge_col);
                    } else {
                        c.draw_icon_wifi(row_rect.x + 28.0, row_rect.y + 27.0, 7.0, badge_col);
                    }

                    let title =
                        self.fonts
                            .truncate_to_width(&dev.display_name(), 12.5, true, iw - 74.0);
                    c.draw_text(
                        &mut self.fonts,
                        &title,
                        row_rect.x + 54.0,
                        row_rect.y + 11.0,
                        12.5,
                        true,
                        ColorRgba::white(0.98),
                    );
                    c.draw_text(
                        &mut self.fonts,
                        dev.connection_label(),
                        row_rect.x + 54.0,
                        row_rect.y + 29.0,
                        10.8,
                        false,
                        ColorRgba::white(if is_sel { 0.84 } else { 0.56 }),
                    );
                    let dot_col = if is_sel {
                        ColorRgba::white(0.96)
                    } else {
                        ColorRgba::rgb(0.24, 0.85, 0.45)
                    };
                    c.fill_circle(row_rect.x + iw - 15.0, row_rect.y + 27.0, 3.5, dot_col);
                    self.hit_regions.push(HitRegion {
                        rect: row_rect,
                        action,
                    });
                }
                ry += row_h + row_gap;
            }
        }

        // Discovered on Wi-Fi (Avahi)
        if nearby_h > 0.0 {
            let mut ny = panel.y + panel.h - pad - footer_h - nearby_h;
            c.draw_text(
                &mut self.fonts,
                "Discovered on Wi-Fi",
                ix,
                ny,
                10.8,
                true,
                ColorRgba::white(0.55),
            );
            ny += 18.0;
            for nearby in unique_nearby.iter().take(3) {
                c.draw_icon_wifi(ix + 7.0, ny + 10.0, 5.5, ColorRgba::rgb(0.45, 0.80, 1.0));
                let name =
                    self.fonts
                        .truncate_to_width(&nearby.display_name(), 11.0, true, iw - 24.0);
                c.draw_text(
                    &mut self.fonts,
                    &name,
                    ix + 20.0,
                    ny + 1.0,
                    11.0,
                    true,
                    ColorRgba::white(0.92),
                );
                c.draw_text(
                    &mut self.fonts,
                    "Avahi / Bonjour active",
                    ix + 20.0,
                    ny + 15.0,
                    9.4,
                    false,
                    ColorRgba::white(0.48),
                );
                ny += 30.0;
            }
        }

        // Footer
        let fy = panel.y + panel.h - pad - 28.0;
        let status_dot = if self.is_manual_scanning {
            ColorRgba::rgb(0.32, 0.72, 1.0)
        } else if self.error.is_none() {
            ColorRgba::rgb(0.24, 0.85, 0.45)
        } else {
            ColorRgba::rgb(1.0, 0.62, 0.22)
        };
        c.fill_circle(ix + 4.0, fy + 6.0, 3.0, status_dot);
        let status_txt = if self.is_manual_scanning {
            "Scanning…"
        } else if self.settings.transport == TransportPreference::Airplay {
            "AirPlay ready"
        } else {
            "Discovery active"
        };
        c.draw_text(
            &mut self.fonts,
            status_txt,
            ix + 12.0,
            fy - 1.0,
            10.5,
            false,
            ColorRgba::white(0.65),
        );
        let ver_label = Self::app_version_label();
        c.draw_text(
            &mut self.fonts,
            &ver_label,
            ix,
            fy + 14.0,
            10.2,
            false,
            ColorRgba::white(0.42),
        );

        let upd_label = self
            .available_update
            .as_ref()
            .map(|u| format!("v{}", u.version));
        let pill_w = if let Some(lbl) = &upd_label {
            self.fonts.measure_text(lbl, 10.0, true) + 26.0
        } else {
            28.0
        };
        let upd_rect = Rect::new(ix + iw - pill_w, fy + 2.0, pill_w, 24.0);
        let upd_hover = self.hovered_action == Some(HitAction::CheckOrOpenUpdate);
        let upd_bg = if self.available_update.is_some() {
            ColorRgba::rgba(0.22, 0.56, 1.0, if upd_hover { 0.82 } else { 0.65 })
        } else {
            ColorRgba::white(if upd_hover { 0.16 } else { 0.08 })
        };
        c.fill_rounded_rect(upd_rect, 12.0, upd_bg);
        c.stroke_rounded_rect(upd_rect, 12.0, 1.0, ColorRgba::white(0.18));
        c.draw_icon_refresh(
            upd_rect.x + 14.0,
            upd_rect.y + 12.0,
            4.5,
            ColorRgba::white(if self.is_checking_updates { 0.45 } else { 0.85 }),
        );
        if let Some(lbl) = &upd_label {
            c.draw_text(
                &mut self.fonts,
                lbl,
                upd_rect.x + 22.0,
                upd_rect.y + 5.0,
                10.0,
                true,
                ColorRgba::white(0.98),
            );
        }
        self.hit_regions.push(HitRegion {
            rect: upd_rect,
            action: HitAction::CheckOrOpenUpdate,
        });
    }

    fn draw_stage(&mut self, c: &mut Canvas, panel: Rect) {
        c.draw_glass_surface(panel, 22.0, 0.05);
        let pad = 32.0;
        let cx = panel.x + panel.w * 0.5;
        let iw = panel.w - pad * 2.0;

        // Update Banner
        if let Some(update) = self.available_update.clone() {
            if !self.update_dismissed {
                let banner = Rect::new(panel.x + pad, panel.y + 22.0, iw, 36.0);
                c.fill_rounded_rect(banner, 12.0, ColorRgba::white(0.08));
                c.stroke_rounded_rect(banner, 12.0, 1.0, ColorRgba::white(0.20));
                c.draw_icon_sparkles(
                    banner.x + 18.0,
                    banner.y + 18.0,
                    5.0,
                    ColorRgba::rgb(0.48, 0.80, 1.0),
                );
                let msg = format!("Update available: v{}", update.version);
                c.draw_text(
                    &mut self.fonts,
                    &msg,
                    banner.x + 30.0,
                    banner.y + 10.0,
                    12.0,
                    true,
                    ColorRgba::white(0.96),
                );

                let close_rect = Rect::new(banner.x + banner.w - 28.0, banner.y + 6.0, 22.0, 24.0);
                c.draw_icon_close(
                    close_rect.x + 11.0,
                    close_rect.y + 12.0,
                    4.0,
                    ColorRgba::white(0.65),
                );
                self.hit_regions.push(HitRegion {
                    rect: close_rect,
                    action: HitAction::DismissUpdate,
                });

                let dl_rect = Rect::new(banner.x + banner.w - 114.0, banner.y + 6.0, 78.0, 24.0);
                let dl_hover = self.hovered_action == Some(HitAction::DownloadUpdate);
                c.fill_rounded_rect(
                    dl_rect,
                    12.0,
                    ColorRgba::rgba(0.25, 0.56, 1.0, if dl_hover { 1.0 } else { 0.85 }),
                );
                c.draw_text_centered(
                    &mut self.fonts,
                    "Download",
                    dl_rect.x + dl_rect.w * 0.5,
                    dl_rect.y + 5.0,
                    11.2,
                    true,
                    ColorRgba::white(0.98),
                );
                self.hit_regions.push(HitRegion {
                    rect: dl_rect,
                    action: HitAction::DownloadUpdate,
                });
            }
        }

        let selected = self.selected_device();
        let stage_cy = panel.y + panel.h * 0.43;

        if self.settings.transport == TransportPreference::Airplay {
            c.draw_radial_glow(cx, stage_cy - 76.0, 8.0, 68.0, ColorRgba::white(0.11));
            c.draw_icon_airplay(cx, stage_cy - 74.0, 60.0, 2.5, ColorRgba::white(0.96));
            c.draw_text_centered(
                &mut self.fonts,
                "AirPlay H.264 Receiver",
                cx,
                stage_cy - 18.0,
                24.0,
                true,
                ColorRgba::white(0.98),
            );
            let desc = "Open Control Center -> Screen Mirroring on your iPhone and select \"iPhone Mirror\".";
            let lines = self.fonts.wrap_lines(desc, 12.8, false, iw.min(390.0));
            for (i, line) in lines.iter().enumerate() {
                c.draw_text_centered(
                    &mut self.fonts,
                    line,
                    cx,
                    stage_cy + 18.0 + i as f32 * 18.0,
                    12.8,
                    false,
                    ColorRgba::white(0.75),
                );
            }

            let pill_text = if let Some(dev) = &selected {
                format!(
                    "Hybrid touch & keyboard enabled via {}",
                    dev.connection_label()
                )
            } else {
                "Video + Audio mode (connect USB/Wi-Fi once to add touch control)".to_string()
            };
            let pill_w = (self.fonts.measure_text(&pill_text, 11.2, true) + 32.0).min(iw);
            let pill_rect = Rect::new(cx - pill_w * 0.5, stage_cy + 64.0, pill_w, 28.0);
            c.fill_rounded_rect(pill_rect, 14.0, ColorRgba::white(0.09));
            c.stroke_rounded_rect(pill_rect, 14.0, 1.0, ColorRgba::white(0.22));
            let truncated = self
                .fonts
                .truncate_to_width(&pill_text, 11.2, true, pill_w - 20.0);
            c.draw_text_centered(
                &mut self.fonts,
                &truncated,
                cx,
                pill_rect.y + 7.0,
                11.2,
                true,
                ColorRgba::white(0.86),
            );

            let btn_title = if self.backend_running {
                "Stop Receiver"
            } else {
                "Start AirPlay Receiver"
            };
            self.draw_primary_button(c, cx, stage_cy + 114.0, btn_title);
        } else if let Some(dev) = selected {
            c.draw_radial_glow(cx, stage_cy - 76.0, 8.0, 72.0, ColorRgba::white(0.11));
            c.draw_icon_iphone(cx, stage_cy - 76.0, 74.0, 2.3, ColorRgba::white(0.96));
            if dev.is_network() {
                c.draw_icon_wifi(
                    cx + 34.0,
                    stage_cy - 102.0,
                    9.0,
                    ColorRgba::rgb(0.45, 0.80, 1.0),
                );
            }
            c.draw_text_centered(
                &mut self.fonts,
                "Ready to mirror",
                cx,
                stage_cy - 14.0,
                24.5,
                true,
                ColorRgba::white(0.98),
            );
            let sub = format!("{} · {} HEVC", dev.display_name(), dev.connection_label());
            let sub_trunc = self.fonts.truncate_to_width(&sub, 12.8, false, iw);
            c.draw_text_centered(
                &mut self.fonts,
                &sub_trunc,
                cx,
                stage_cy + 20.0,
                12.8,
                false,
                ColorRgba::white(0.72),
            );

            let btn_title = if self.backend_running {
                "Disconnect"
            } else {
                "Start Mirroring"
            };
            self.draw_primary_button(c, cx, stage_cy + 62.0, btn_title);

            let has_usb = self.devices.iter().any(|d| d.is_usb());
            if !self.backend_running
                && (has_usb || self.settings.transport == TransportPreference::Wifi)
            {
                let pair_label = if self.pairing_in_progress {
                    "Pairing Wi-Fi…"
                } else {
                    "Pair Wi-Fi via USB"
                };
                let pw = self.fonts.measure_text(pair_label, 11.5, true) + 28.0;
                let pair_rect = Rect::new(cx - pw * 0.5, stage_cy + 114.0, pw, 28.0);
                let hover = self.hovered_action == Some(HitAction::PairWifi);
                c.fill_rounded_rect(
                    pair_rect,
                    14.0,
                    ColorRgba::white(if hover && has_usb { 0.14 } else { 0.08 }),
                );
                c.stroke_rounded_rect(pair_rect, 14.0, 1.0, ColorRgba::white(0.20));
                c.draw_text_centered(
                    &mut self.fonts,
                    pair_label,
                    cx,
                    pair_rect.y + 7.0,
                    11.5,
                    true,
                    ColorRgba::white(if has_usb { 0.86 } else { 0.45 }),
                );
                if has_usb && !self.pairing_in_progress {
                    self.hit_regions.push(HitRegion {
                        rect: pair_rect,
                        action: HitAction::PairWifi,
                    });
                }
            }
        } else {
            c.draw_icon_iphone(
                cx - 10.0,
                stage_cy - 68.0,
                56.0,
                2.0,
                ColorRgba::white(0.55),
            );
            c.draw_icon_iphone(
                cx + 12.0,
                stage_cy - 62.0,
                56.0,
                2.0,
                ColorRgba::white(0.35),
            );
            c.draw_text_centered(
                &mut self.fonts,
                "Connect an iPhone",
                cx,
                stage_cy - 8.0,
                24.0,
                true,
                ColorRgba::white(0.98),
            );
            let desc = "Plug in USB for the lowest latency, use paired Wi-Fi, or switch the mode selector to AirPlay.";
            let lines = self.fonts.wrap_lines(desc, 13.0, false, iw.min(390.0));
            for (i, line) in lines.iter().enumerate() {
                c.draw_text_centered(
                    &mut self.fonts,
                    line,
                    cx,
                    stage_cy + 28.0 + i as f32 * 18.0,
                    13.0,
                    false,
                    ColorRgba::white(0.70),
                );
            }
            let has_usb = self.devices.iter().any(|d| d.is_usb());
            if has_usb || self.settings.transport == TransportPreference::Wifi {
                let pair_label = if self.pairing_in_progress {
                    "Pairing Wi-Fi…"
                } else {
                    "Pair Wi-Fi via USB"
                };
                let pw = self.fonts.measure_text(pair_label, 11.5, true) + 28.0;
                let pair_rect = Rect::new(cx - pw * 0.5, stage_cy + 86.0, pw, 28.0);
                let hover = self.hovered_action == Some(HitAction::PairWifi);
                c.fill_rounded_rect(
                    pair_rect,
                    14.0,
                    ColorRgba::white(if hover && has_usb { 0.14 } else { 0.08 }),
                );
                c.stroke_rounded_rect(pair_rect, 14.0, 1.0, ColorRgba::white(0.20));
                c.draw_text_centered(
                    &mut self.fonts,
                    pair_label,
                    cx,
                    pair_rect.y + 7.0,
                    11.5,
                    true,
                    ColorRgba::white(if has_usb { 0.86 } else { 0.45 }),
                );
                if has_usb && !self.pairing_in_progress {
                    self.hit_regions.push(HitRegion {
                        rect: pair_rect,
                        action: HitAction::PairWifi,
                    });
                }
            }
        }

        // Error or Status Notice Banner
        let banner_msg = self
            .error
            .as_ref()
            .map(|e| (true, e.clone()))
            .or_else(|| self.status_notice.as_ref().map(|s| (false, s.clone())));
        if let Some((is_err, text)) = banner_msg {
            let max_bw = iw.min(430.0);
            let lines = self.fonts.wrap_lines(&text, 11.8, false, max_bw - 28.0);
            let line_count = lines.len().min(3);
            let bh = 16.0 + line_count as f32 * 16.0;
            let by = panel.y + panel.h - pad - bh;
            let brect = Rect::new(cx - max_bw * 0.5, by, max_bw, bh);
            if is_err {
                c.fill_rounded_rect(brect, 10.0, ColorRgba::rgba(0.95, 0.22, 0.24, 0.14));
                c.stroke_rounded_rect(brect, 10.0, 1.0, ColorRgba::rgba(1.0, 0.32, 0.34, 0.34));
            } else {
                c.fill_rounded_rect(brect, 10.0, ColorRgba::rgba(0.24, 0.62, 1.0, 0.14));
                c.stroke_rounded_rect(brect, 10.0, 1.0, ColorRgba::rgba(0.35, 0.72, 1.0, 0.34));
            }
            let text_col = if is_err {
                ColorRgba::rgb(1.0, 0.56, 0.56)
            } else {
                ColorRgba::rgb(0.62, 0.86, 1.0)
            };
            for (i, line) in lines.iter().take(3).enumerate() {
                c.draw_text_centered(
                    &mut self.fonts,
                    line,
                    cx,
                    brect.y + 8.0 + i as f32 * 16.0,
                    11.8,
                    false,
                    text_col,
                );
            }
        }
    }

    fn draw_notifications_stage(&mut self, c: &mut Canvas, panel: Rect) {
        c.draw_glass_surface(panel, 20.0, 0.06);
        let pad = 22.0;
        let ix = panel.x + pad;
        let iw = panel.w - pad * 2.0;
        let mut y = panel.y + pad;

        c.draw_icon_bell(ix + 10.0, y + 10.0, 6.5, ColorRgba::rgb(0.32, 0.68, 1.0));
        c.draw_text(
            &mut self.fonts,
            "iPhone Notifications",
            ix + 24.0,
            y + 1.0,
            16.5,
            true,
            ColorRgba::white(0.98),
        );

        let close_rect = Rect::new(ix + iw - 24.0, y, 24.0, 24.0);
        c.fill_circle(
            close_rect.x + 12.0,
            close_rect.y + 12.0,
            12.0,
            ColorRgba::white(
                if self.hovered_action == Some(HitAction::ToggleNotificationsPanel) {
                    0.18
                } else {
                    0.08
                },
            ),
        );
        c.draw_icon_close(
            close_rect.x + 12.0,
            close_rect.y + 12.0,
            4.2,
            ColorRgba::white(0.85),
        );
        self.hit_regions.push(HitRegion {
            rect: close_rect,
            action: HitAction::ToggleNotificationsPanel,
        });

        y += 38.0;
        let status_card = Rect::new(ix, y, iw, 92.0);
        c.fill_rounded_rect(status_card, 12.0, ColorRgba::white(0.06));
        c.stroke_rounded_rect(status_card, 12.0, 1.0, ColorRgba::white(0.11));

        let bluez_line = self
            .fonts
            .truncate_to_width(&self.bluez_status, 12.0, true, iw - 24.0);
        c.draw_text(
            &mut self.fonts,
            &bluez_line,
            ix + 12.0,
            y + 12.0,
            12.0,
            true,
            ColorRgba::white(0.92),
        );
        let hint = self.fonts.truncate_to_width(
            "Pair iPhone via Bluetooth and enable Share System Notifications in iPhone Settings.",
            11.0,
            false,
            iw - 24.0,
        );
        c.draw_text(
            &mut self.fonts,
            &hint,
            ix + 12.0,
            y + 32.0,
            11.0,
            false,
            ColorRgba::white(0.62),
        );

        let buttons = [
            (
                "Send Test Notification",
                HitAction::SendTestNotification,
                160.0,
            ),
            ("Solicit ANCS BLE", HitAction::SolicitBluezAncs, 134.0),
            ("Clear History", HitAction::ClearNotifications, 110.0),
        ];
        let mut bx = ix + 12.0;
        for (label, action, w) in buttons {
            let r = Rect::new(bx, y + 54.0, w, 26.0);
            let hover = self.hovered_action.as_ref() == Some(&action);
            c.fill_rounded_rect(r, 8.0, ColorRgba::white(if hover { 0.16 } else { 0.09 }));
            c.stroke_rounded_rect(r, 8.0, 1.0, ColorRgba::white(0.18));
            c.draw_text_centered(
                &mut self.fonts,
                label,
                r.x + r.w * 0.5,
                r.y + 6.5,
                11.0,
                true,
                ColorRgba::white(0.92),
            );
            self.hit_regions.push(HitRegion { rect: r, action });
            bx += w + 8.0;
        }

        y += 106.0;
        let toggles = [
            (
                "Enable Notifications",
                self.settings.notifications.enabled,
                HitAction::ToggleNotificationsEnabled,
            ),
            (
                "Mute While Mirroring",
                self.settings.notifications.mute_while_mirroring,
                HitAction::ToggleMuteWhileMirroring,
            ),
            (
                "Hide Previews",
                self.settings.notifications.hide_previews,
                HitAction::ToggleHidePreviews,
            ),
        ];
        let chip_w = (iw - 16.0) / 3.0;
        for (i, (label, on, action)) in toggles.into_iter().enumerate() {
            let r = Rect::new(ix + i as f32 * (chip_w + 8.0), y, chip_w, 28.0);
            c.fill_rounded_rect(
                r,
                8.0,
                if on {
                    ColorRgba::rgba(0.24, 0.55, 1.0, 0.35)
                } else {
                    ColorRgba::white(0.06)
                },
            );
            c.stroke_rounded_rect(r, 8.0, 1.0, ColorRgba::white(if on { 0.34 } else { 0.14 }));
            c.draw_text_centered(
                &mut self.fonts,
                label,
                r.x + r.w * 0.5,
                r.y + 7.5,
                11.0,
                on,
                ColorRgba::white(if on { 0.96 } else { 0.72 }),
            );
            self.hit_regions.push(HitRegion { rect: r, action });
        }

        y += 42.0;
        if self.notification_history.is_empty() {
            c.draw_text_centered(
                &mut self.fonts,
                "No notifications yet — click 'Send Test Notification' to test FreeDesktop alerts.",
                panel.x + panel.w * 0.5,
                y + 70.0,
                12.0,
                false,
                ColorRgba::white(0.55),
            );
            return;
        }

        for notif in self.notification_history.iter().take(5) {
            if y + 54.0 > panel.y + panel.h - pad {
                break;
            }
            let row = Rect::new(ix, y, iw, 50.0);
            c.fill_rounded_rect(row, 10.0, ColorRgba::white(0.06));
            c.stroke_rounded_rect(row, 10.0, 1.0, ColorRgba::white(0.10));
            let headline = format!("{} • {}", notif.app_display_name, notif.title);
            let h_trunc = self
                .fonts
                .truncate_to_width(&headline, 12.0, true, iw - 24.0);
            c.draw_text(
                &mut self.fonts,
                &h_trunc,
                ix + 12.0,
                y + 9.0,
                12.0,
                true,
                ColorRgba::white(0.95),
            );
            let b_trunc = self
                .fonts
                .truncate_to_width(&notif.body, 11.2, false, iw - 24.0);
            c.draw_text(
                &mut self.fonts,
                &b_trunc,
                ix + 12.0,
                y + 27.0,
                11.2,
                false,
                ColorRgba::white(0.72),
            );
            y += 58.0;
        }
    }

    fn draw_primary_button(&mut self, c: &mut Canvas, cx: f32, y: f32, title: &str) {
        let tw = self.fonts.measure_text(title, 14.0, true);
        let bw = (tw + 66.0).max(176.0);
        let bh = 40.0;
        let rect = Rect::new(cx - bw * 0.5, y, bw, bh);
        let hover = self.hovered_action == Some(HitAction::PrimaryAction);
        let boost = if hover { 0.08 } else { 0.0 };
        c.draw_shadow_rounded_rect(rect, 20.0, 12.0, 0.36);

        let (tl, br) = if self.backend_running {
            (
                ColorRgba::rgb(0.92 + boost, 0.30, 0.34),
                ColorRgba::rgb(0.78 + boost, 0.18, 0.24),
            )
        } else {
            (
                ColorRgba::rgb(0.28 + boost, 0.62 + boost, 1.0),
                ColorRgba::rgb(0.16 + boost, 0.44 + boost, 0.95),
            )
        };
        c.fill_rounded_rect_gradient(rect, 20.0, tl, br);
        c.stroke_rounded_rect(rect, 20.0, 1.0, ColorRgba::white(0.38));

        let content_w = 18.0 + tw;
        let start_x = cx - content_w * 0.5;
        if self.backend_running {
            c.draw_icon_stop(
                start_x + 6.0,
                rect.y + bh * 0.5,
                5.5,
                ColorRgba::white(0.98),
            );
        } else {
            c.draw_icon_play(
                start_x + 6.0,
                rect.y + bh * 0.5,
                6.0,
                ColorRgba::white(0.98),
            );
        }
        c.draw_text(
            &mut self.fonts,
            title,
            start_x + 19.0,
            rect.y + 11.0,
            14.0,
            true,
            ColorRgba::white(0.99),
        );
        self.hit_regions.push(HitRegion {
            rect,
            action: HitAction::PrimaryAction,
        });
    }

    fn draw_settings_modal(&mut self, c: &mut Canvas, lw: f32, lh: f32) {
        c.fill_rounded_rect(Rect::new(0.0, 0.0, lw, lh), 0.0, ColorRgba::black(0.54));
        // Clear underlying hitboxes so clicks inside modal only interact with modal
        self.hit_regions.clear();

        let mw = 470.0_f32.min(lw - 40.0);
        let mh = 326.0_f32.min(lh - 40.0);
        let modal = Rect::new((lw - mw) * 0.5, (lh - mh) * 0.5, mw, mh);
        c.draw_glass_surface(modal, 20.0, 0.09);

        let pad = 24.0;
        let ix = modal.x + pad;
        let iw = modal.w - pad * 2.0;
        let mut y = modal.y + 20.0;

        c.draw_text(
            &mut self.fonts,
            "Settings",
            ix,
            y,
            16.5,
            true,
            ColorRgba::white(0.98),
        );
        let close_rect = Rect::new(ix + iw - 26.0, y - 2.0, 26.0, 26.0);
        c.fill_circle(
            close_rect.x + 13.0,
            close_rect.y + 13.0,
            12.0,
            ColorRgba::white(if self.hovered_action == Some(HitAction::CloseSettings) {
                0.18
            } else {
                0.08
            }),
        );
        c.draw_icon_close(
            close_rect.x + 13.0,
            close_rect.y + 13.0,
            4.2,
            ColorRgba::white(0.85),
        );
        self.hit_regions.push(HitRegion {
            rect: close_rect,
            action: HitAction::CloseSettings,
        });

        y += 38.0;
        c.draw_text(
            &mut self.fonts,
            "Connection",
            ix,
            y,
            12.5,
            true,
            ColorRgba::white(0.82),
        );
        y += 22.0;

        let modes = [
            (TransportPreference::Auto, "Automatic"),
            (TransportPreference::Usb, "USB only"),
            (TransportPreference::Wifi, "Wi-Fi only"),
            (TransportPreference::Airplay, "AirPlay"),
        ];
        let seg_w = (iw - 12.0) / 4.0;
        for (i, (mode, label)) in modes.iter().enumerate() {
            let r = Rect::new(ix + i as f32 * (seg_w + 4.0), y, seg_w, 30.0);
            let sel = self.settings.transport == *mode;
            c.fill_rounded_rect(
                r,
                8.0,
                if sel {
                    ColorRgba::rgba(0.24, 0.54, 1.0, 0.80)
                } else {
                    ColorRgba::white(0.08)
                },
            );
            c.stroke_rounded_rect(r, 8.0, 1.0, ColorRgba::white(if sel { 0.35 } else { 0.14 }));
            c.draw_text_centered(
                &mut self.fonts,
                label,
                r.x + r.w * 0.5,
                r.y + 8.0,
                11.2,
                sel,
                ColorRgba::white(if sel { 0.98 } else { 0.78 }),
            );
            self.hit_regions.push(HitRegion {
                rect: r,
                action: HitAction::SettingsSelectTransport(*mode),
            });
        }

        y += 46.0;
        let toggles = [
            (
                "Connect automatically when an iPhone appears",
                self.settings.auto_connect,
                HitAction::ToggleAutoConnect,
            ),
            (
                "Resize mirror window when orientation changes",
                self.settings.auto_rotate,
                HitAction::ToggleAutoRotate,
            ),
            (
                "Check for updates on launch",
                self.settings.auto_check_updates,
                HitAction::ToggleAutoCheckUpdates,
            ),
        ];
        for (label, enabled, action) in toggles {
            let row = Rect::new(ix, y, iw, 30.0);
            let box_rect = Rect::new(ix, y + 6.0, 18.0, 18.0);
            c.fill_rounded_rect(
                box_rect,
                5.0,
                if enabled {
                    ColorRgba::rgba(0.25, 0.58, 1.0, 0.92)
                } else {
                    ColorRgba::white(0.08)
                },
            );
            c.stroke_rounded_rect(box_rect, 5.0, 1.0, ColorRgba::white(0.26));
            if enabled {
                c.draw_icon_check(
                    box_rect.x + 9.0,
                    box_rect.y + 9.0,
                    5.5,
                    ColorRgba::white(0.98),
                );
            }
            c.draw_text(
                &mut self.fonts,
                label,
                ix + 28.0,
                y + 7.0,
                12.2,
                false,
                ColorRgba::white(0.92),
            );
            self.hit_regions.push(HitRegion { rect: row, action });
            y += 34.0;
        }

        y += 12.0;
        let check_btn = Rect::new(ix, y, 134.0, 30.0);
        c.fill_rounded_rect(check_btn, 9.0, ColorRgba::white(0.10));
        c.stroke_rounded_rect(check_btn, 9.0, 1.0, ColorRgba::white(0.20));
        c.draw_text_centered(
            &mut self.fonts,
            if self.is_checking_updates {
                "Checking…"
            } else {
                "Check for Updates"
            },
            check_btn.x + check_btn.w * 0.5,
            check_btn.y + 8.0,
            11.5,
            true,
            ColorRgba::white(0.95),
        );
        self.hit_regions.push(HitRegion {
            rect: check_btn,
            action: HitAction::SettingsCheckUpdates,
        });

        if let Some(msg) = &self.update_status_message {
            let trunc = self.fonts.truncate_to_width(msg, 11.0, false, iw - 220.0);
            c.draw_text(
                &mut self.fonts,
                &trunc,
                ix + 146.0,
                y + 8.0,
                11.0,
                false,
                ColorRgba::white(0.68),
            );
        }

        let done_btn = Rect::new(ix + iw - 72.0, y, 72.0, 30.0);
        c.fill_rounded_rect(done_btn, 9.0, ColorRgba::rgba(0.24, 0.55, 1.0, 0.85));
        c.stroke_rounded_rect(done_btn, 9.0, 1.0, ColorRgba::white(0.32));
        c.draw_text_centered(
            &mut self.fonts,
            "Done",
            done_btn.x + done_btn.w * 0.5,
            done_btn.y + 8.0,
            11.8,
            true,
            ColorRgba::white(0.98),
        );
        self.hit_regions.push(HitRegion {
            rect: done_btn,
            action: HitAction::CloseSettings,
        });

        let ver = Self::app_version_label();
        c.draw_text_centered(
            &mut self.fonts,
            &ver,
            modal.x + modal.w * 0.5,
            modal.y + modal.h - 22.0,
            10.5,
            false,
            ColorRgba::white(0.45),
        );
    }

    fn hit_test(&self, lx: f32, ly: f32) -> Option<HitAction> {
        self.hit_regions
            .iter()
            .rev()
            .find(|region| region.rect.contains(lx, ly))
            .map(|region| region.action.clone())
    }
}

impl ApplicationHandler<LauncherEvent> for LauncherApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("iPhone Mirror")
            .with_inner_size(LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT))
            .with_min_inner_size(LogicalSize::new(MIN_WIDTH, MIN_HEIGHT));
        match event_loop.create_window(attrs) {
            Ok(window) => match MetalRenderer::new(&window) {
                Ok(renderer) => {
                    self.renderer = Some(renderer);
                    self.window = Some(window);
                    self.trigger_scan(false);
                    if self.settings.auto_check_updates {
                        self.trigger_update_check(true);
                    }
                    let scan_proxy = self.proxy.clone();
                    tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(4));
                        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        interval.tick().await;
                        loop {
                            interval.tick().await;
                            if scan_proxy.send_event(LauncherEvent::ScanRequested).is_err() {
                                break;
                            }
                        }
                    });
                    self.render();
                }
                Err(err) => {
                    tracing::error!(%err, "Failed to initialize Linux launcher renderer");
                    event_loop.exit();
                }
            },
            Err(err) => {
                tracing::error!(%err, "Failed to create launcher window");
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: LauncherEvent) {
        match event {
            LauncherEvent::ScanRequested => self.trigger_scan(false),
            LauncherEvent::DevicesScanned { manual, result } => {
                self.is_scanning = false;
                if manual {
                    self.is_manual_scanning = false;
                }
                match result {
                    Ok(mut scanned) => {
                        scanned.sort_by(|a, b| {
                            if a.is_usb() != b.is_usb() {
                                return (!a.is_usb()).cmp(&(!b.is_usb()));
                            }
                            a.udid.cmp(&b.udid)
                        });
                        self.devices = scanned;
                        if !self.backend_running && !self.pairing_in_progress {
                            self.error = None;
                        }
                        self.sync_selection();
                        if self.settings.auto_connect
                            && !self.did_auto_connect
                            && !self.backend_running
                            && self.settings.transport != TransportPreference::Airplay
                            && self.selected_device().is_some()
                        {
                            self.did_auto_connect = true;
                            self.start_mirror_session();
                        }
                    }
                    Err(msg) => {
                        if !self.devices.is_empty() {
                            self.devices.clear();
                        }
                        if self.settings.transport != TransportPreference::Airplay {
                            self.error = Some(msg);
                        }
                    }
                }
                self.request_redraw();
            }
            LauncherEvent::NearbyScanned(nearby) => {
                if self.nearby_devices != nearby {
                    self.nearby_devices = nearby;
                    self.request_redraw();
                }
            }
            LauncherEvent::UpdateChecked {
                silent,
                result,
                status,
            } => {
                self.is_checking_updates = false;
                match result {
                    Ok(Some(update)) => {
                        self.available_update = Some(update);
                        self.update_dismissed = false;
                        self.update_status_message = Some(status);
                    }
                    Ok(None) => {
                        self.available_update = None;
                        if !silent {
                            self.update_status_message = Some(status);
                        }
                    }
                    Err(msg) => {
                        if !silent {
                            self.update_status_message = Some(msg);
                        }
                    }
                }
                self.request_redraw();
            }
            LauncherEvent::BackendExited {
                exit_ok,
                error_detail,
            } => {
                self.backend_running = false;
                self.backend_cancel = None;
                self.status_notice = None;
                if let Some(window) = &self.window {
                    window.focus_window();
                }
                if !exit_ok {
                    self.error = Some(error_detail.unwrap_or_else(|| {
                        "The mirror session ended. Keep the iPhone unlocked, trusted, and on the same network, then reconnect.".to_string()
                    }));
                }
                self.request_redraw();
            }
            LauncherEvent::PairFinished(res) => {
                self.pairing_in_progress = false;
                match res {
                    Ok(msg) => {
                        self.status_notice = Some(msg);
                        self.error = None;
                        self.trigger_scan(false);
                    }
                    Err(err) => {
                        self.status_notice = None;
                        self.error = Some(err);
                    }
                }
                self.request_redraw();
            }
            LauncherEvent::BluezStatusUpdated(detail) => {
                self.bluez_status = detail;
                self.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.stop_mirror_session();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &self.renderer {
                    renderer.resize(size.width, size.height);
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.render();
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(window) = &self.window {
                    let logical: LogicalPosition<f32> = position.to_logical(window.scale_factor());
                    self.cursor_logical = Some((logical.x, logical.y));
                    let next_hover = self.hit_test(logical.x, logical.y);
                    if next_hover != self.hovered_action {
                        window.set_cursor(if next_hover.is_some() {
                            CursorIcon::Pointer
                        } else {
                            CursorIcon::Default
                        });
                        self.hovered_action = next_hover;
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some((lx, ly)) = self.cursor_logical {
                    if let Some(action) = self.hit_test(lx, ly) {
                        self.handle_action(action);
                    } else if self.show_settings {
                        self.show_settings = false;
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 24.0,
                    MouseScrollDelta::PixelDelta(pos) => pos.y as f32,
                };
                if (self.cursor_logical.map(|(x, _)| x).unwrap_or(0.0)) < SIDEBAR_WIDTH + OUTER_PAD
                {
                    self.device_scroll = (self.device_scroll - dy).max(0.0);
                    self.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) if self.show_settings => {
                        self.show_settings = false;
                        self.request_redraw();
                    }
                    Key::Named(NamedKey::Enter) if !self.show_settings => {
                        self.handle_action(HitAction::PrimaryAction);
                    }
                    Key::Character(ch)
                        if self.modifiers.control_key() && ch.eq_ignore_ascii_case("r") =>
                    {
                        self.trigger_scan(true);
                    }
                    Key::Character(ch) if self.modifiers.control_key() && ch.as_str() == "," => {
                        self.show_settings = !self.show_settings;
                        self.request_redraw();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

pub fn run_launcher() -> Result<()> {
    let event_loop = EventLoop::<LauncherEvent>::with_user_event()
        .build()
        .context("Failed to create launcher event loop")?;
    let proxy = event_loop.create_proxy();
    let mut app = LauncherApp::new(proxy);
    event_loop
        .run_app(&mut app)
        .context("Launcher event loop failed")?;
    Ok(())
}

async fn scan_devices_json() -> Result<Vec<LauncherDevice>, String> {
    let exe = env::current_exe().map_err(|e| format!("Failed to find executable: {e}"))?;
    let output = tokio::process::Command::new(exe)
        .args(["devices", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Couldn't scan for iPhones: {e}"))?;
    if !output.status.success() {
        return Err(
            "Couldn't scan for iPhones. Check that usbmuxd / Apple Mobile Device services are running."
                .to_string(),
        );
    }
    serde_json::from_slice::<Vec<LauncherDevice>>(&output.stdout)
        .map_err(|e| format!("Invalid device list JSON: {e}"))
}

async fn scan_nearby_avahi() -> Vec<NearbyDevice> {
    let mut found = Vec::new();
    for service in ["_remotepairing._tcp", "_apple-mobdev2._tcp"] {
        let Ok(Ok(output)) = tokio::time::timeout(
            Duration::from_millis(900),
            tokio::process::Command::new("avahi-browse")
                .args(["-pt", service])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output(),
        )
        .await
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut names = BTreeSet::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split(';').collect();
            if fields.len() >= 4 && (fields[0] == "+" || fields[0] == "=") {
                let name = fields[3].trim();
                if !name.is_empty() {
                    names.insert(name.to_string());
                }
            }
        }
        for name in names {
            found.push(NearbyDevice {
                name,
                service: service.to_string(),
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

async fn check_github_updates() -> (Result<Option<AvailableUpdate>, String>, String) {
    let output = match tokio::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "10",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: iphone-mirror",
            GITHUB_RELEASES_API,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(out) => out,
        Err(_) => {
            return (
                Err("Update check failed. Check your internet connection.".to_string()),
                "Update check failed.".to_string(),
            )
        }
    };
    if !output.status.success() {
        return (
            Err("Update check failed. Check your internet connection.".to_string()),
            "Could not check updates.".to_string(),
        );
    }
    let Ok(release) = serde_json::from_slice::<GitHubRelease>(&output.stdout) else {
        return (
            Err("Could not parse GitHub release response.".to_string()),
            "Invalid release response.".to_string(),
        );
    };
    let current = env!("CARGO_PKG_VERSION");
    let local_beta = current.to_ascii_lowercase().contains("beta");
    let clean_remote = release
        .tag_name
        .trim()
        .trim_start_matches(['v', 'V'])
        .to_string();
    if is_newer_version(&release.tag_name, current, local_beta) {
        let msg = format!("Update available: v{clean_remote}");
        (
            Ok(Some(AvailableUpdate {
                version: clean_remote,
                url: release.html_url,
            })),
            msg,
        )
    } else {
        (Ok(None), format!("You're up to date (v{current})."))
    }
}

async fn run_pair_subcommand(udid: &str) -> Result<String, String> {
    let exe = env::current_exe().map_err(|e| format!("Failed to find executable: {e}"))?;
    let output = tokio::process::Command::new(exe)
        .args(["pair", "--udid", udid])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run pairing command: {e}"))?;
    if output.status.success() {
        Ok("Paired! Wi-Fi mirroring is now available.".to_string())
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(if !err.is_empty() {
            err
        } else if !out.is_empty() {
            out
        } else {
            "Wi-Fi pairing failed. Unlock your iPhone and accept Trust.".to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_device(
        udid: &str,
        conn: &str,
        name: Option<&str>,
        model: Option<&str>,
    ) -> LauncherDevice {
        LauncherDevice {
            udid: udid.to_string(),
            connection_type: conn.to_string(),
            device_id: 1,
            host: None,
            port: None,
            name: name.map(str::to_string),
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn semver_comparison_handles_upgrades_and_betas() {
        assert!(is_newer_version("v0.5.0", "0.4.0", false));
        assert!(!is_newer_version("v0.4.0", "0.4.0", false));
        assert!(!is_newer_version("v0.3.9", "0.4.0", false));
        assert!(is_newer_version("v0.4.0", "0.4.0", true));
        assert!(!is_newer_version("v0.4.0-beta.2", "0.4.0", true));
    }

    #[test]
    fn auto_transport_deduplicates_by_udid_and_prefers_usb() {
        let devices = vec![
            sample_device(
                "00008110-AAA",
                "usb",
                Some("Arifian's iPhone"),
                Some("iPhone 15"),
            ),
            sample_device(
                "00008110-AAA",
                "network",
                Some("Arifian's iPhone"),
                Some("iPhone 15"),
            ),
            sample_device("00008110-BBB", "network", Some("Second iPhone"), None),
        ];
        let auto = filter_devices_for_transport(&devices, TransportPreference::Auto);
        assert_eq!(auto.len(), 2);
        assert!(auto[0].is_usb());
        assert_eq!(auto[1].udid, "00008110-BBB");

        let wifi_only = filter_devices_for_transport(&devices, TransportPreference::Wifi);
        assert_eq!(wifi_only.len(), 2);
        assert!(wifi_only.iter().all(LauncherDevice::is_network));
    }

    #[test]
    fn display_name_formats_user_and_model_and_skips_uuid() {
        let named = sample_device("123", "usb", Some("Arifian"), Some("iPhone SE"));
        assert_eq!(named.display_name(), "Arifian · iPhone SE");

        let uuid_name = sample_device(
            "123",
            "network",
            Some("00008110-001A2B3C4D5E6F70"),
            Some("iPhone 16"),
        );
        assert_eq!(uuid_name.display_name(), "iPhone (Wi-Fi) · iPhone 16");
    }
}

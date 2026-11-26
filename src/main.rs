// language: Rust, file: src/main.rs, target: macOS (Hackintosh / Mac)
use anyhow::{Context, Result};
use idevice::{
    core_device::{
        ButtonState, DataInclusionPolicy, IndigoHidClient, MainKeyboardService,
        PasteboardServiceClient, UniversalHidServiceClient, GENERAL_PASTEBOARD,
    },
    lockdown::LockdownClient,
    springboardservices::{InterfaceOrientation, SpringBoardServicesClient},
    IdeviceService, RsdService,
};
use iphone_mirror_rs::{
    airplay::AirPlayReceiver,
    audio::{
        AacEldDecoder,
        jitter::AudioJitter,
        output::{
            AudioOutput, AudioRestartReason, AudioTelemetry, create_output, diagnostics_enabled,
        },
        reorder::{AudioPacket, AudioReorder, ReorderedAudio},
    },
    codec::{
        decoder::{DecodedFrame, HardwareDecoder},
        format_desc::FormatDescription,
        screenshot::save_frame_png,
    },
    input::{
        clipboard::{ClipboardContent, SystemClipboard, MAX_CLIPBOARD_IMAGE_BYTES},
        coordinate_mapper::{CoordinateMapper, DisplayRotation},
    },
    media::nal_parser::{NalUnitType, ParameterSetCache},
    media::{h265_depacketizer::H265Depacketizer, rtp::RtpPacket},
    protocol::{display_client::DisplayClient, rtcp_stats::RtcpReportStats},
    transfer::{
        afc::AfcTransfer,
        manager::{TransferCommand, TransferEvent, TransferManager},
    },
    transport::{
        ddi_mounter::DdiMounter,
        device_scanner::{ConnectedDevice, DeviceScanner, TransportPreference},
        rsd_tunnel::RsdTunnel,
        wifi::{self, WifiDevice},
    },
    ui::metal_renderer::MetalRenderer,
};
use std::collections::BTreeSet;
use std::env;
use std::io::Write;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tracing::{debug, info, warn};

enum Command {
    Mirror,
    Airplay,
    Devices { json: bool },
    Diagnose { udid: Option<String> },
    Pair { udid: Option<String> },
    ProbeFiles { udid: Option<String> },
    SendFiles {
        udid: Option<String>,
        paths: Vec<std::path::PathBuf>,
    },
    Help,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AppSettings {
    #[serde(default)]
    transport: TransportPreference,
    #[serde(default = "default_true")]
    auto_rotate: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            transport: TransportPreference::Auto,
            auto_rotate: true,
        }
    }
}

fn default_true() -> bool {
    true
}

enum UiEvent {
    FrameReady,
    Orientation(DisplayRotation),
    Clipboard(ClipboardContent),
    ShutdownRequested,
    ShutdownComplete,
    StreamError(String),
    Transfer(TransferEvent),
}

#[derive(Debug, Clone)]
enum InputCommand {
    Down(u16, u16),
    Move(u16, u16),
    Up(u16, u16),
    ScrollStart(u16, u16),
    ScrollMove(i32, i32),
    ScrollEnd,
    SetKeys(Vec<u16>),
    /// iOS has no hardware Back button; emulate the left-edge swipe gesture.
    NavigateBack(DisplayRotation),
}

/// Consumer-page (0x0C) HID usages for the iPhone's physical buttons.
#[derive(Debug, Clone, Copy)]
enum HardwareButton {
    Home,
    Lock,
    VolumeUp,
    VolumeDown,
    Mute,
}

impl HardwareButton {
    fn usage(self) -> (u64, u64) {
        match self {
            Self::Home => (0x0C, 0x40),
            Self::Lock => (0x0C, 0x30),
            Self::VolumeUp => (0x0C, 0xE9),
            Self::VolumeDown => (0x0C, 0xEA),
            Self::Mute => (0x0C, 0xE2),
        }
    }

    /// iOS rejects near-instant taps for the side button; everything else
    /// behaves like a normal 50 ms tap.
    fn hold(self) -> std::time::Duration {
        match self {
            Self::Lock => std::time::Duration::from_millis(500),
            _ => std::time::Duration::from_millis(50),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Lock => "lock",
            Self::VolumeUp => "volume-up",
            Self::VolumeDown => "volume-down",
            Self::Mute => "mute",
        }
    }
}

struct MirrorApp {
    window: Option<winit::window::Window>,
    renderer: Option<MetalRenderer>,
    latest_frame: Arc<std::sync::Mutex<Option<DecodedFrame>>>,
    frame_pending: Arc<AtomicBool>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    frame_count: u64,
    frame_size: Option<(usize, usize)>,
    cursor_position: Option<winit::dpi::PhysicalPosition<f64>>,
    touch_active: bool,
    scroll_active: bool,
    scroll_deadline: Option<std::time::Instant>,
    /// Synthetic touch is only started after enough movement accumulated, so a
    /// tiny scroll flick does not become a tap on the iPhone.
    scroll_started: bool,
    scroll_accum: (f64, f64),
    scroll_anchor: (u16, u16),
    /// A two-finger scroll may end with the trackpad registering a small tap
    /// (especially on Hackintosh touchpads). Ignore left clicks briefly after
    /// a scroll so lifting the fingers does not fire an accidental tap.
    suppress_click_until: Option<std::time::Instant>,
    last_touch_position: Option<(u16, u16)>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputCommand>,
    button_tx: tokio::sync::mpsc::UnboundedSender<HardwareButton>,
    audio_muted: Arc<AtomicBool>,
    screenshot_requested: bool,
    always_on_top: bool,
    fullscreen: bool,
    auto_rotate: bool,
    rotation: DisplayRotation,
    manual_rotation_override: bool,
    frame_is_landscape: Option<bool>,
    pressed_keys: BTreeSet<u16>,
    modifiers: winit::keyboard::ModifiersState,
    clipboard: SystemClipboard,
    clipboard_tx: tokio::sync::mpsc::UnboundedSender<ClipboardContent>,
    last_device_clipboard: Option<String>,
    transfer_tx: Option<tokio::sync::mpsc::UnboundedSender<TransferCommand>>,
    drop_candidates: Vec<std::path::PathBuf>,
    drop_deadline: Option<std::time::Instant>,
    transfer_status: Option<String>,
    stream_error: Option<String>,
    shutting_down: bool,
}

impl winit::application::ApplicationHandler<UiEvent> for MirrorApp {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(250),
        ));
        if self.window.is_some() {
            return;
        }
        let attributes = winit::window::Window::default_attributes()
            .with_title("iPhone Mirror")
            .with_inner_size(winit::dpi::LogicalSize::new(420, 750))
            .with_min_inner_size(winit::dpi::LogicalSize::new(280, 500));
        match event_loop.create_window(attributes) {
            Ok(window) => match MetalRenderer::new(&window) {
                Ok(renderer) => {
                    self.renderer = Some(renderer);
                    self.window = Some(window);
                }
                Err(error) => {
                    tracing::error!(%error, "Failed to initialize Metal");
                    event_loop.exit();
                }
            },
            Err(error) => {
                tracing::error!(%error, "Failed to create mirror window");
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::FrameReady => {
                self.frame_pending.store(false, Ordering::Release);
                let Some(frame) = self
                    .latest_frame
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take())
                else {
                    return;
                };
                self.frame_count += 1;
                self.frame_size = Some((frame.width(), frame.height()));
                let landscape = self.rotation.is_landscape() || frame.width() > frame.height();
                if self.auto_rotate
                    && self.frame_is_landscape != Some(landscape)
                    && !self.fullscreen
                {
                    self.frame_is_landscape = Some(landscape);
                    if let Some(window) = &self.window {
                        let current = window.inner_size();
                        let (width, height) =
                            rotated_window_size(current.width, current.height, landscape);
                        let target = winit::dpi::PhysicalSize::new(width, height);
                        let _ = window.request_inner_size(target);
                        info!(
                            landscape,
                            width = target.width,
                            height = target.height,
                            "Auto-rotated mirror window"
                        );
                    }
                }
                if self.frame_count == 1 || self.frame_count % 300 == 0 {
                    info!(
                        frame_count = self.frame_count,
                        width = frame.width(),
                        height = frame.height(),
                        "Presenting Metal frame"
                    );
                }
                if let Some(renderer) = &self.renderer {
                    if std::mem::take(&mut self.screenshot_requested) {
                        match default_screenshot_path() {
                            Ok(path) => match save_frame_png(&frame, &path) {
                                Ok(()) => info!(path = %path.display(), "Saved screenshot"),
                                Err(error) => warn!(%error, "Failed to save screenshot"),
                            },
                            Err(error) => {
                                warn!(%error, "Failed to prepare the screenshot directory")
                            }
                        }
                    }
                    if let Err(error) = renderer.render_frame(&frame) {
                        warn!(%error, "Failed to present decoded frame");
                    }
                }
            }
            UiEvent::Orientation(rotation) => {
                if self.auto_rotate && !self.manual_rotation_override && self.rotation != rotation {
                    self.apply_rotation(rotation);
                }
            }
            UiEvent::Transfer(event) => {
                use TransferEvent::*;
                match event {
                    Started { files } => {
                        self.transfer_status = Some(format!("Sending {files} file(s)…"));
                    }
                    Progress {
                        name,
                        sent,
                        total,
                        file_index,
                        file_count,
                    } => {
                        let percent = if total == 0 {
                            100
                        } else {
                            (sent * 100 / total).min(100)
                        };
                        self.transfer_status = Some(format!(
                            "Sending {name} ({percent}%) — {file_index}/{file_count}"
                        ));
                    }
                    FileStaged { name, .. } => {
                        info!(%name, "File staged on iPhone");
                    }
                    Published { files } => {
                        info!(files, "File URLs published on iPhone pasteboard");
                    }
                    Completed { files } => {
                        self.transfer_status = Some(format!(
                            "Sent {files} file(s) — paste on iPhone (⌘V)"
                        ));
                    }
                    Failed { name, message } => {
                        warn!(?name, %message, "File transfer failed");
                        self.transfer_status = Some(format!("Transfer failed: {message}"));
                    }
                }
                self.update_window_title();
            }
            UiEvent::StreamError(error) => {
                tracing::error!(%error, "Display stream stopped");
                self.stream_error = Some(error);
                event_loop.exit();
            }
            UiEvent::ShutdownRequested => self.begin_shutdown(),
            UiEvent::ShutdownComplete => {
                info!("Mirror session cleanup complete");
                event_loop.exit();
            }
            UiEvent::Clipboard(content) => match content {
                ClipboardContent::Text(text) => {
                    if self.clipboard.current_text().as_deref() != Some(text.as_str()) {
                        if let Err(error) = self.clipboard.set_text(&text) {
                            warn!(%error, "Failed to copy iPhone clipboard text to macOS");
                        } else {
                            info!(bytes = text.len(), "Copied iPhone clipboard text to macOS");
                        }
                    }
                    self.last_device_clipboard = Some(text);
                }
                ClipboardContent::Image { uti, bytes } => {
                    if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
                        warn!(
                            bytes = bytes.len(),
                            "Ignoring oversized iPhone clipboard image"
                        );
                    } else if let Err(error) = self.clipboard.set_image(&uti, &bytes) {
                        warn!(%error, "Failed to copy iPhone clipboard image to macOS");
                    } else {
                        info!(%uti, bytes = bytes.len(), "Copied iPhone clipboard image to macOS");
                    }
                    self.last_device_clipboard = None;
                }
            },
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        match event {
            winit::event::WindowEvent::CloseRequested => {
                self.begin_shutdown();
            }
            winit::event::WindowEvent::Resized(size) => {
                if let Some(renderer) = &self.renderer {
                    renderer.resize(size.width, size.height);
                }
            }
            winit::event::WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some(position);
                if self.touch_active {
                    if let Some((x, y)) = self.touch_coordinates(position) {
                        self.last_touch_position = Some((x, y));
                        let _ = self.input_tx.send(InputCommand::Move(x, y));
                    }
                }
            }
            winit::event::WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => match state {
                winit::event::ElementState::Pressed => {
                    if self.scroll_active
                        || self
                            .suppress_click_until
                            .is_some_and(|deadline| std::time::Instant::now() < deadline)
                    {
                        debug!("Ignoring click during/after a trackpad scroll");
                        return;
                    }
                    if let Some(position) = self.cursor_position {
                        if let Some((x, y)) = self.touch_coordinates(position) {
                            self.touch_active = true;
                            self.last_touch_position = Some((x, y));
                            let _ = self.input_tx.send(InputCommand::Down(x, y));
                        }
                    }
                }
                winit::event::ElementState::Released => {
                    if self.touch_active {
                        self.touch_active = false;
                        let release = self
                            .cursor_position
                            .and_then(|position| self.touch_coordinates(position))
                            .or(self.last_touch_position.take());
                        if let Some((x, y)) = release {
                            let _ = self.input_tx.send(InputCommand::Up(x, y));
                        }
                    }
                }
            },
            winit::event::WindowEvent::CursorLeft { .. } => {
                if self.touch_active {
                    self.touch_active = false;
                    if let Some((x, y)) = self.last_touch_position.take() {
                        let _ = self.input_tx.send(InputCommand::Up(x, y));
                    }
                }
                self.end_scroll();
            }
            winit::event::WindowEvent::MouseWheel { delta, phase, .. } => {
                let (x, y) = match delta {
                    // Trackpads report physical pixels. Keep the sign that macOS
                    // supplies so natural-scrolling follows the system setting.
                    winit::event::MouseScrollDelta::PixelDelta(delta) => (delta.x, delta.y),
                    // Wheel mice report abstract lines; 24 points per line gives
                    // a useful iOS scroll distance without large jumps.
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        (x as f64 * 24.0, y as f64 * 24.0)
                    }
                };
                if phase == winit::event::TouchPhase::Started || !self.scroll_active {
                    self.scroll_anchor = self
                        .cursor_position
                        .and_then(|position| self.touch_coordinates(position))
                        .unwrap_or((u16::MAX / 2, u16::MAX / 2));
                    self.scroll_active = true;
                    self.scroll_started = false;
                    self.scroll_accum = (0.0, 0.0);
                }
                if x != 0.0 || y != 0.0 {
                    self.scroll_accum.0 += x;
                    self.scroll_accum.1 += y;

                    const START_THRESHOLD: f64 = 6.0; // screen pixels
                    if !self.scroll_started {
                        let magnitude = self.scroll_accum.0.hypot(self.scroll_accum.1);
                        if magnitude < START_THRESHOLD {
                            // Let the gesture gather momentum first; a touch down
                            // here would read as a tap when the fingers lift.
                            if matches!(
                                phase,
                                winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled
                            ) {
                                self.end_scroll();
                            }
                            return;
                        }
                        let _ = self.input_tx.send(InputCommand::ScrollStart(
                            self.scroll_anchor.0,
                            self.scroll_anchor.1,
                        ));
                        self.scroll_started = true;
                    }

                    // Scale trackpad pixels into the 0...65535 touchscreen
                    // coordinate domain. Positive wheel motion is positive
                    // finger motion, preserving the macOS natural-scroll setting.
                    let (ax, ay) = self.scroll_accum;
                    self.scroll_accum = (0.0, 0.0);
                    let scale = self
                        .window
                        .as_ref()
                        .map(|window| {
                            let size = window.inner_size();
                            (
                                u16::MAX as f64 / size.width.max(1) as f64,
                                u16::MAX as f64 / size.height.max(1) as f64,
                            )
                        })
                        .unwrap_or((100.0, 100.0));
                    let (dx, dy) = self
                        .rotation
                        .rotate_delta_f64(ax * scale.0, ay * scale.1);
                    let _ = self.input_tx.send(InputCommand::ScrollMove(
                        dx.round() as i32,
                        dy.round() as i32,
                    ));
                    self.scroll_deadline =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(250));
                }
                if matches!(
                    phase,
                    winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled
                ) {
                    self.end_scroll();
                }
            }
            winit::event::WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            winit::event::WindowEvent::KeyboardInput { event, .. } => {
                if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        winit::keyboard::KeyCode::ArrowUp
                        | winit::keyboard::KeyCode::ArrowDown => {
                            match event.state {
                                winit::event::ElementState::Pressed => {
                                    let direction = if code == winit::keyboard::KeyCode::ArrowUp {
                                        1
                                    } else {
                                        -1
                                    };
                                    self.arrow_scroll(direction);
                                }
                                winit::event::ElementState::Released => self.end_scroll(),
                            }
                            return;
                        }
                        winit::keyboard::KeyCode::Escape => {
                            if event.state == winit::event::ElementState::Pressed && !event.repeat
                            {
                                self.end_scroll();
                                let _ = self
                                    .input_tx
                                    .send(InputCommand::NavigateBack(self.rotation));
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                if event.state == winit::event::ElementState::Pressed && !event.repeat {
                    if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key {
                        if self.modifiers.super_key() && code == winit::keyboard::KeyCode::Comma {
                            let mut settings = load_settings();
                            if let Err(error) = edit_settings(&mut settings) {
                                warn!(%error, "Failed to save settings");
                            } else {
                                self.auto_rotate = settings.auto_rotate;
                                info!(
                                    transport = settings.transport.label(),
                                    auto_rotate = settings.auto_rotate,
                                    "Updated application settings"
                                );
                            }
                            return;
                        }
                        let control = self.modifiers.control_key()
                            || self.pressed_keys.contains(&0xE0)
                            || self.pressed_keys.contains(&0xE4);
                        if control {
                            if code == winit::keyboard::KeyCode::KeyM {
                                let was_muted = self.audio_muted.fetch_xor(true, Ordering::Relaxed);
                                info!(muted = !was_muted, "Toggled host audio mute");
                                return;
                            }
                            if code == winit::keyboard::KeyCode::KeyS {
                                self.screenshot_requested = true;
                                return;
                            }
                            if code == winit::keyboard::KeyCode::KeyT {
                                self.always_on_top = !self.always_on_top;
                                if let Some(window) = &self.window {
                                    window.set_window_level(if self.always_on_top {
                                        winit::window::WindowLevel::AlwaysOnTop
                                    } else {
                                        winit::window::WindowLevel::Normal
                                    });
                                }
                                info!(always_on_top = self.always_on_top, "Toggled always-on-top");
                                return;
                            }
                            if code == winit::keyboard::KeyCode::KeyR {
                                let next = self.rotation.cycle_manual();
                                self.manual_rotation_override = next != DisplayRotation::Portrait;
                                self.apply_rotation(next);
                                info!(rotation = next.label(), "Rotated mirror display manually");
                                return;
                            }
                            if code == winit::keyboard::KeyCode::KeyF && self.modifiers.super_key()
                            {
                                self.fullscreen = !self.fullscreen;
                                if let Some(window) = &self.window {
                                    window.set_fullscreen(if self.fullscreen {
                                        Some(winit::window::Fullscreen::Borderless(None))
                                    } else {
                                        None
                                    });
                                }
                                info!(fullscreen = self.fullscreen, "Toggled fullscreen");
                                return;
                            }
                            let button = match code {
                                winit::keyboard::KeyCode::KeyH => Some(HardwareButton::Home),
                                winit::keyboard::KeyCode::KeyL => Some(HardwareButton::Lock),
                                winit::keyboard::KeyCode::BracketLeft => {
                                    Some(HardwareButton::VolumeDown)
                                }
                                winit::keyboard::KeyCode::BracketRight => {
                                    Some(HardwareButton::VolumeUp)
                                }
                                winit::keyboard::KeyCode::Backslash => Some(HardwareButton::Mute),
                                _ => None,
                            };
                            if let Some(button) = button {
                                let _ = self.button_tx.send(button);
                                return;
                            }
                        }
                    }
                }
                if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key {
                    if let Some(usage) = key_code_to_hid_usage(code) {
                        let changed = match event.state {
                            winit::event::ElementState::Pressed => self.pressed_keys.insert(usage),
                            winit::event::ElementState::Released => {
                                self.pressed_keys.remove(&usage)
                            }
                        };
                        if changed {
                            let _ = self.input_tx.send(InputCommand::SetKeys(
                                self.pressed_keys.iter().copied().collect(),
                            ));
                        }
                    }
                }
            }
            winit::event::WindowEvent::Focused(false) => {
                if !self.pressed_keys.is_empty() {
                    self.pressed_keys.clear();
                    let _ = self.input_tx.send(InputCommand::SetKeys(Vec::new()));
                }
                self.end_scroll();
            }
            winit::event::WindowEvent::HoveredFile(path) => {
                if !self.drop_candidates.contains(&path) {
                    self.drop_candidates.push(path);
                }
                self.transfer_status = Some(format!(
                    "Drop {} file(s) to send to iPhone",
                    self.drop_candidates.len()
                ));
                self.update_window_title();
            }
            winit::event::WindowEvent::HoveredFileCancelled => {
                self.drop_candidates.clear();
                self.drop_deadline = None;
                self.transfer_status = None;
                self.update_window_title();
            }
            winit::event::WindowEvent::DroppedFile(path) => {
                if !self.drop_candidates.contains(&path) {
                    self.drop_candidates.push(path);
                }
                // winit sends one event per file; batch a multi-file drop.
                self.drop_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(150));
                self.transfer_status = Some(format!(
                    "Preparing {} file(s)…",
                    self.drop_candidates.len()
                ));
                self.update_window_title();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let now = std::time::Instant::now();
        if self.scroll_active && self.scroll_deadline.is_some_and(|deadline| now >= deadline) {
            self.end_scroll();
        }
        if self
            .drop_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.drop_deadline = None;
            let paths = std::mem::take(&mut self.drop_candidates);
            if !paths.is_empty() {
                if let Some(transfer_tx) = &self.transfer_tx {
                    if transfer_tx.send(TransferCommand::SendFiles(paths)).is_ok() {
                        info!("Queued dropped files for transfer");
                    } else {
                        self.transfer_status = Some("File transfer unavailable".to_string());
                        self.update_window_title();
                    }
                }
            }
        }
        if let Some(content) = self.clipboard.changed_content() {
            let is_device_text = matches!(
                &content,
                ClipboardContent::Text(text)
                    if self.last_device_clipboard.as_deref() == Some(text.as_str())
            );
            if !is_device_text {
                let _ = self.clipboard_tx.send(content);
            }
        }
        let clipboard_deadline = now + std::time::Duration::from_millis(250);
        let next = std::iter::once(clipboard_deadline)
            .chain(self.scroll_deadline)
            .chain(self.drop_deadline)
            .min()
            .unwrap_or(clipboard_deadline);
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(next));
    }
}

fn device_switch_enabled() -> bool {
    matches!(
        env::var("IPHONE_MIRROR_AUDIO_DEVICE_SWITCH").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn key_code_to_hid_usage(code: winit::keyboard::KeyCode) -> Option<u16> {
    use winit::keyboard::KeyCode;
    Some(match code {
        KeyCode::KeyA => 0x04,
        KeyCode::KeyB => 0x05,
        KeyCode::KeyC => 0x06,
        KeyCode::KeyD => 0x07,
        KeyCode::KeyE => 0x08,
        KeyCode::KeyF => 0x09,
        KeyCode::KeyG => 0x0A,
        KeyCode::KeyH => 0x0B,
        KeyCode::KeyI => 0x0C,
        KeyCode::KeyJ => 0x0D,
        KeyCode::KeyK => 0x0E,
        KeyCode::KeyL => 0x0F,
        KeyCode::KeyM => 0x10,
        KeyCode::KeyN => 0x11,
        KeyCode::KeyO => 0x12,
        KeyCode::KeyP => 0x13,
        KeyCode::KeyQ => 0x14,
        KeyCode::KeyR => 0x15,
        KeyCode::KeyS => 0x16,
        KeyCode::KeyT => 0x17,
        KeyCode::KeyU => 0x18,
        KeyCode::KeyV => 0x19,
        KeyCode::KeyW => 0x1A,
        KeyCode::KeyX => 0x1B,
        KeyCode::KeyY => 0x1C,
        KeyCode::KeyZ => 0x1D,
        KeyCode::Digit1 => 0x1E,
        KeyCode::Digit2 => 0x1F,
        KeyCode::Digit3 => 0x20,
        KeyCode::Digit4 => 0x21,
        KeyCode::Digit5 => 0x22,
        KeyCode::Digit6 => 0x23,
        KeyCode::Digit7 => 0x24,
        KeyCode::Digit8 => 0x25,
        KeyCode::Digit9 => 0x26,
        KeyCode::Digit0 => 0x27,
        KeyCode::Enter => 0x28,
        KeyCode::Escape => 0x29,
        KeyCode::Backspace => 0x2A,
        KeyCode::Tab => 0x2B,
        KeyCode::Space => 0x2C,
        KeyCode::Minus => 0x2D,
        KeyCode::Equal => 0x2E,
        KeyCode::BracketLeft => 0x2F,
        KeyCode::BracketRight => 0x30,
        KeyCode::Backslash => 0x31,
        KeyCode::Semicolon => 0x33,
        KeyCode::Quote => 0x34,
        KeyCode::Backquote => 0x35,
        KeyCode::Comma => 0x36,
        KeyCode::Period => 0x37,
        KeyCode::Slash => 0x38,
        KeyCode::CapsLock => 0x39,
        KeyCode::F1 => 0x3A,
        KeyCode::F2 => 0x3B,
        KeyCode::F3 => 0x3C,
        KeyCode::F4 => 0x3D,
        KeyCode::F5 => 0x3E,
        KeyCode::F6 => 0x3F,
        KeyCode::F7 => 0x40,
        KeyCode::F8 => 0x41,
        KeyCode::F9 => 0x42,
        KeyCode::F10 => 0x43,
        KeyCode::F11 => 0x44,
        KeyCode::F12 => 0x45,
        KeyCode::Insert => 0x49,
        KeyCode::Home => 0x4A,
        KeyCode::PageUp => 0x4B,
        KeyCode::Delete => 0x4C,
        KeyCode::End => 0x4D,
        KeyCode::PageDown => 0x4E,
        KeyCode::ArrowRight => 0x4F,
        KeyCode::ArrowLeft => 0x50,
        KeyCode::ArrowDown => 0x51,
        KeyCode::ArrowUp => 0x52,
        KeyCode::ControlLeft => 0xE0,
        KeyCode::ShiftLeft => 0xE1,
        KeyCode::AltLeft => 0xE2,
        KeyCode::SuperLeft => 0xE3,
        KeyCode::ControlRight => 0xE4,
        KeyCode::ShiftRight => 0xE5,
        KeyCode::AltRight => 0xE6,
        KeyCode::SuperRight => 0xE7,
        _ => return None,
    })
}

impl MirrorApp {
    fn update_window_title(&self) {
        if let Some(window) = &self.window {
            let title = match &self.transfer_status {
                Some(status) => format!("iPhone Mirror — {status}"),
                None => "iPhone Mirror".to_string(),
            };
            window.set_title(&title);
        }
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        info!("Gracefully stopping the mirror session");
        let _ = self.shutdown_tx.send(true);
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
    }

    fn apply_rotation(&mut self, rotation: DisplayRotation) {
        self.rotation = rotation;
        if let Some(renderer) = &self.renderer {
            renderer.set_rotation(rotation);
        }
        let landscape = rotation.is_landscape()
            || self.frame_size.is_some_and(|(width, height)| width > height);
        if !self.fullscreen && self.frame_is_landscape != Some(landscape) {
            self.frame_is_landscape = Some(landscape);
            if let Some(window) = &self.window {
                let current = window.inner_size();
                let (width, height) =
                    rotated_window_size(current.width, current.height, landscape);
                let target = winit::dpi::PhysicalSize::new(width, height);
                let _ = window.request_inner_size(target);
                info!(
                    rotation = rotation.label(),
                    landscape,
                    width = target.width,
                    height = target.height,
                    "Rotated mirror window"
                );
            }
        }
    }

    /// Scroll the mirrored iPhone with a synthetic touchscreen drag. The finger
    /// moves opposite to the content: ArrowUp reveals content above (finger
    /// drags down), ArrowDown reveals content below (finger drags up).
    fn arrow_scroll(&mut self, direction: i32) {
        let anchor = self
            .cursor_position
            .and_then(|position| self.touch_coordinates(position))
            .unwrap_or((u16::MAX / 2, u16::MAX / 2));
        if !self.scroll_active {
            self.scroll_active = true;
            self.scroll_started = true;
            self.scroll_accum = (0.0, 0.0);
            self.scroll_anchor = anchor;
            let _ = self
                .input_tx
                .send(InputCommand::ScrollStart(anchor.0, anchor.1));
        }
        let step = (u16::MAX as f32 * 0.22 * direction as f32).round() as i32;
        let (dx, dy) = self.rotation.rotate_delta(0, step);
        let _ = self.input_tx.send(InputCommand::ScrollMove(dx, dy));
        self.scroll_deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
    }

    fn end_scroll(&mut self) {
        if self.scroll_active {
            self.scroll_active = false;
            self.scroll_deadline = None;
            // Only lift a synthetic finger if one was actually placed. A tiny
            // scroll that never crossed the threshold becomes a no-op instead
            // of a tap.
            if self.scroll_started {
                let _ = self.input_tx.send(InputCommand::ScrollEnd);
            }
            self.scroll_started = false;
            self.scroll_accum = (0.0, 0.0);
            // Suppress the phantom click some trackpads emit when the fingers
            // are lifted at the end of a two-finger scroll.
            self.suppress_click_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(400));
        }
    }

    fn touch_coordinates(&self, position: winit::dpi::PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let window = self.window.as_ref()?;
        let (frame_width, frame_height) = self.frame_size?;
        let size = window.inner_size();
        let (x, y) = CoordinateMapper::map_aspect_fit_rotated(
            size.width as f64,
            size.height as f64,
            frame_width as f64,
            frame_height as f64,
            self.rotation,
            position.x,
            position.y,
        )?;
        Some((
            (x * u16::MAX as f32).round() as u16,
            (y * u16::MAX as f32).round() as u16,
        ))
    }
}

fn default_screenshot_path() -> Result<std::path::PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    let directory = std::path::PathBuf::from(home)
        .join("Pictures")
        .join("iPhone Mirror");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("Failed to create {}", directory.display()))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    Ok(directory.join(format!("iphone-mirror-{millis}.png")))
}

fn offset_touch_position(position: (u16, u16), delta: (i32, i32)) -> (u16, u16) {
    (
        (position.0 as i32 + delta.0).clamp(0, u16::MAX as i32) as u16,
        (position.1 as i32 + delta.1).clamp(0, u16::MAX as i32) as u16,
    )
}

fn rotated_window_size(width: u32, height: u32, landscape: bool) -> (u32, u32) {
    let long = width.max(height).max(500);
    let short = width.min(height).max(280);
    if landscape {
        (long, short)
    } else {
        (short, long)
    }
}

fn settings_path() -> Result<std::path::PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(std::path::PathBuf::from(home)
        .join("Library/Application Support/iPhone Mirror/settings.json"))
}

fn load_settings() -> AppSettings {
    settings_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_settings(settings: &AppSettings) -> Result<()> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(settings)?)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LauncherAction {
    Retry,
    Settings,
    Quit,
}

fn launcher_alert(message: &str, detail: &str) -> LauncherAction {
    const SCRIPT: &str = r#"
on run argv
  set result to display dialog (item 2 of argv) with title (item 1 of argv) buttons {"Quit", "Settings…", "Try Again"} default button "Try Again" cancel button "Quit" with icon caution
  return button returned of result
end run
"#;
    match std::process::Command::new("osascript")
        .args(["-e", SCRIPT, "--", message, detail])
        .output()
    {
        Ok(output) if output.status.success() => {
            match String::from_utf8_lossy(&output.stdout).trim() {
                "Try Again" => LauncherAction::Retry,
                "Settings…" => LauncherAction::Settings,
                _ => LauncherAction::Quit,
            }
        }
        _ => LauncherAction::Quit,
    }
}

fn edit_settings(settings: &mut AppSettings) -> Result<()> {
    loop {
        let transport = format!("Connection: {}", settings.transport.label());
        let rotation = format!(
            "Auto-Rotate: {}",
            if settings.auto_rotate { "On" } else { "Off" }
        );
        const SCRIPT: &str = r#"
on run argv
  set result to display dialog "Auto prefers USB when the same iPhone is also available over Wi-Fi." with title "iPhone Mirror Settings" buttons {"Done", item 2 of argv, item 1 of argv} default button "Done"
  return button returned of result
end run
"#;
        let output = std::process::Command::new("osascript")
            .args(["-e", SCRIPT, "--", &transport, &rotation])
            .output()?;
        if !output.status.success() {
            break;
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            value if value == transport => {
                settings.transport = match settings.transport {
                    TransportPreference::Auto => TransportPreference::Usb,
                    TransportPreference::Usb => TransportPreference::Wifi,
                    TransportPreference::Wifi => TransportPreference::Airplay,
                    TransportPreference::Airplay => TransportPreference::Auto,
                };
            }
            value if value == rotation => settings.auto_rotate = !settings.auto_rotate,
            _ => break,
        }
    }
    save_settings(settings)
}

fn parse_command() -> Result<Command> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        None | Some("mirror") => Ok(Command::Mirror),
        Some("airplay") => Ok(Command::Airplay),
        Some("devices") => {
            let json = args.any(|arg| arg == "--json");
            Ok(Command::Devices { json })
        }
        Some("diagnose") => {
            let mut udid = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--udid" => {
                        udid = Some(args.next().context("--udid requires a value")?);
                    }
                    _ => anyhow::bail!("Unknown diagnose option: {arg}"),
                }
            }
            Ok(Command::Diagnose { udid })
        }
        Some("pair") => {
            let mut udid = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--udid" => {
                        udid = Some(args.next().context("--udid requires a value")?);
                    }
                    _ => anyhow::bail!("Unknown pair option: {arg}"),
                }
            }
            Ok(Command::Pair { udid })
        }
        Some("probe-files") => {
            let mut udid = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--udid" => {
                        udid = Some(args.next().context("--udid requires a value")?);
                    }
                    _ => anyhow::bail!("Unknown probe-files option: {arg}"),
                }
            }
            Ok(Command::ProbeFiles { udid })
        }
        Some("send-files") => {
            let mut udid = None;
            let mut paths = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--udid" => {
                        udid = Some(args.next().context("--udid requires a value")?);
                    }
                    other => paths.push(std::path::PathBuf::from(other)),
                }
            }
            if paths.is_empty() {
                anyhow::bail!("send-files requires at least one path");
            }
            Ok(Command::SendFiles { udid, paths })
        }
        Some("help" | "--help" | "-h") => Ok(Command::Help),
        Some(command) => anyhow::bail!("Unknown command: {command}. Run with --help for usage."),
    }
}

fn print_help() {
    println!(
        "iphone-mirror-rs\n\n\
         Usage:\n  \
           iphone-mirror [mirror]\n  \
           iphone-mirror airplay\n  \
           iphone-mirror devices\n  \
           iphone-mirror diagnose [--udid <UDID>]\n  \
           iphone-mirror pair [--udid <UDID>]\n  \
           iphone-mirror probe-files [--udid <UDID>]\n  \
           iphone-mirror send-files [--udid <UDID>] <path>...\n"
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iphone_mirror_rs=info,iphone_mirror=info".into()),
        )
        .init();

    match parse_command()? {
        Command::Mirror => supervise_mirror().await,
        Command::Airplay => run_airplay_mirror(load_settings()).await,
        Command::Devices { json } => list_devices(json).await,
        Command::Diagnose { udid } => diagnose(udid.as_deref()).await,
        Command::Pair { udid } => pair_device(udid.as_deref()).await,
        Command::ProbeFiles { udid } => probe_files(udid.as_deref()).await,
        Command::SendFiles { udid, paths } => send_files(udid.as_deref(), paths).await,
        Command::Help => {
            print_help();
            Ok(())
        }
    }
}

#[derive(serde::Serialize)]
struct DeviceJson {
    udid: String,
    connection_type: &'static str,
    device_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct CachedDeviceMeta {
    name: Option<String>,
    model: Option<String>,
}

fn devices_cache_path() -> Option<std::path::PathBuf> {
    env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join("Library/Application Support/iPhone Mirror/devices.json")
    })
}

fn load_device_cache() -> std::collections::BTreeMap<String, CachedDeviceMeta> {
    devices_cache_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_device_cache(cache: &std::collections::BTreeMap<String, CachedDeviceMeta>) {
    let Some(path) = devices_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(cache) {
        let _ = std::fs::write(path, bytes);
    }
}

fn product_type_to_model(product_type: &str) -> String {
    match product_type {
        "iPhone8,4" => "iPhone SE".to_string(),
        "iPhone12,1" => "iPhone 11".to_string(),
        "iPhone12,3" => "iPhone 11 Pro".to_string(),
        "iPhone12,5" => "iPhone 11 Pro Max".to_string(),
        "iPhone12,8" => "iPhone SE (2nd Gen)".to_string(),
        "iPhone13,1" => "iPhone 12 mini".to_string(),
        "iPhone13,2" => "iPhone 12".to_string(),
        "iPhone13,3" => "iPhone 12 Pro".to_string(),
        "iPhone13,4" => "iPhone 12 Pro Max".to_string(),
        "iPhone14,2" => "iPhone 13 Pro".to_string(),
        "iPhone14,3" => "iPhone 13 Pro Max".to_string(),
        "iPhone14,4" => "iPhone 13 mini".to_string(),
        "iPhone14,5" => "iPhone 13".to_string(),
        "iPhone14,6" => "iPhone SE (3rd Gen)".to_string(),
        "iPhone14,7" => "iPhone 14".to_string(),
        "iPhone14,8" => "iPhone 14 Plus".to_string(),
        "iPhone15,2" => "iPhone 14 Pro".to_string(),
        "iPhone15,3" => "iPhone 14 Pro Max".to_string(),
        "iPhone15,4" => "iPhone 15".to_string(),
        "iPhone15,5" => "iPhone 15 Plus".to_string(),
        "iPhone16,1" => "iPhone 15 Pro".to_string(),
        "iPhone16,2" => "iPhone 15 Pro Max".to_string(),
        "iPhone17,1" => "iPhone 16 Pro".to_string(),
        "iPhone17,2" => "iPhone 16 Pro Max".to_string(),
        "iPhone17,3" => "iPhone 16".to_string(),
        "iPhone17,4" => "iPhone 16 Plus".to_string(),
        "iPhone17,5" => "iPhone 16e".to_string(),
        "iPhone18,1" => "iPhone 17 Pro".to_string(),
        "iPhone18,2" => "iPhone 17 Pro Max".to_string(),
        "iPhone18,3" => "iPhone 17".to_string(),
        "iPhone18,4" => "iPhone 17 Air".to_string(),
        other => other.to_string(),
    }
}

async fn read_lockdown_identity(client: &mut LockdownClient) -> CachedDeviceMeta {
    let name = client
        .get_value(Some("DeviceName"), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_string()));
    let model = client
        .get_value(Some("ProductType"), None)
        .await
        .ok()
        .and_then(|v| v.as_string().map(product_type_to_model));
    CachedDeviceMeta { name, model }
}

async fn query_and_cache_rsd_identity(tunnel: &mut RsdTunnel, keys: &[String]) {
    let Ok(Ok(mut lockdown)) = tokio::time::timeout(
        std::time::Duration::from_millis(1200),
        LockdownClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    else {
        return;
    };
    let meta = read_lockdown_identity(&mut lockdown).await;
    if meta.name.is_some() || meta.model.is_some() {
        let mut cache = load_device_cache();
        for key in keys {
            if !key.is_empty() {
                cache.insert(key.clone(), meta.clone());
            }
        }
        save_device_cache(&cache);
    }
}

async fn list_devices(json: bool) -> Result<()> {
    let mut cache = load_device_cache();
    let mut cache_dirty = false;

    let scanned = DeviceScanner::list_devices()
        .await
        .context("Could not scan usbmuxd devices")?;
    let mut devices: Vec<DeviceJson> = Vec::with_capacity(scanned.len());

    for device in scanned {
        let mut meta = cache.get(&device.udid).cloned().unwrap_or_default();
        if let Ok(Ok(mut lockdown)) = tokio::time::timeout(
            std::time::Duration::from_millis(600),
            LockdownClient::connect(&device.provider()),
        )
        .await
        {
            let live = read_lockdown_identity(&mut lockdown).await;
            if live.name.is_some() || live.model.is_some() {
                meta = CachedDeviceMeta {
                    name: live.name.or(meta.name),
                    model: live.model.or(meta.model),
                };
                cache.insert(device.udid.clone(), meta.clone());
                cache_dirty = true;
            }
        }
        devices.push(DeviceJson {
            udid: device.udid,
            connection_type: match device.connection_type {
                iphone_mirror_rs::transport::device_scanner::ConnectionType::Usb => "usb",
                iphone_mirror_rs::transport::device_scanner::ConnectionType::Network => "network",
                iphone_mirror_rs::transport::device_scanner::ConnectionType::Unknown(_) => "unknown",
            },
            device_id: device.device_id,
            host: None,
            port: None,
            name: meta.name,
            model: meta.model,
        });
    }

    match wifi::WifiDiscovery::discover().await {
        Ok(remote) => {
            for device in remote {
                debug!(name = %device.name, host = %device.host, port = device.port, "Found Wi-Fi iPhone");
                let host_key = device.host.to_ascii_lowercase();
                let cached = cache
                    .get(&device.identifier)
                    .or_else(|| cache.get(&host_key));
                let fallback_name = wifi::friendly_name_from_host(&device.host, &device.name);
                let resolved_name = cached
                    .and_then(|c| c.name.clone())
                    .filter(|s| !wifi::is_uuid_like(s))
                    .unwrap_or(fallback_name);
                let resolved_model = cached.and_then(|c| c.model.clone());
                devices.push(DeviceJson {
                    udid: device.identifier,
                    connection_type: "network",
                    device_id: 0,
                    host: Some(device.host),
                    port: Some(device.port),
                    name: Some(resolved_name),
                    model: resolved_model,
                });
            }
        }
        Err(error) => debug!(%error, "Wi-Fi discovery failed"),
    }

    if cache_dirty {
        save_device_cache(&cache);
    }

    if json {
        println!("{}", serde_json::to_string(&devices)?);
        return Ok(());
    }
    if devices.is_empty() {
        println!("No connected iPhones found.");
        return Ok(());
    }

    for device in devices {
        match (device.host, device.port) {
            (Some(host), Some(port)) => println!(
                "{}\t{}\t{host}:{port}",
                device.udid, device.connection_type
            ),
            _ => println!(
                "{}\t{}\tdevice-id={}",
                device.udid, device.connection_type, device.device_id
            ),
        }
    }
    Ok(())
}

async fn pair_device(udid: Option<&str>) -> Result<()> {
    let device = DeviceScanner::select_device(udid).await?;
    println!(
        "Pairing with {} over USB. Accept the prompt on the iPhone if it appears.",
        device.udid
    );
    wifi::pair_via_usb(&device.provider()).await?;
    println!("Paired. Wi-Fi mirroring is now available for this Mac.");
    Ok(())
}

/// Phase 0 probe: stage a small file over AFC, verify its bytes, publish its
/// file URL on the device pasteboard, then clean up.
async fn probe_files(udid: Option<&str>) -> Result<()> {
    use iphone_mirror_rs::transfer::{
        afc::AfcTransfer,
        metadata::{file_url, sanitize_file_name},
        pasteboard::UrlPasteboard,
    };
    use tokio::io::AsyncWriteExt;
    use uuid::Uuid;

    let mut wifi_device = wifi::endpoint_from_env();
    if wifi_device.is_none() {
        wifi_device = wifi::WifiDiscovery::discover()
            .await
            .ok()
            .and_then(|devices| {
                devices
                    .into_iter()
                    .find(|device| udid.is_none() || device.identifier == udid.unwrap_or_default())
            });
    }
    let usb_device = if wifi_device.is_none() {
        Some(DeviceScanner::select_device(udid).await?)
    } else {
        None
    };
    let source = if let Some(wifi_device) = &wifi_device {
        info!(host = %wifi_device.host, port = wifi_device.port, "Probing over Wi-Fi");
        TunnelSource::Wifi(wifi_device)
    } else {
        TunnelSource::Usb(usb_device.as_ref().expect("USB device selected"))
    };

    let mut tunnel = source.connect().await.context("CoreDevice/RSD connect failed")?;
    let mut afc = AfcTransfer::connect(&mut tunnel.adapter, &mut tunnel.handshake)
        .await
        .context("AFC file service is unavailable on this device")?;

    let name = sanitize_file_name(&format!("iphone-mirror-probe-{}.txt", Uuid::new_v4()));
    let local = std::env::temp_dir().join(&name);
    let payload = b"iphone-mirror file-transfer probe\n";
    let mut file = tokio::fs::File::create(&local).await?;
    file.write_all(payload).await?;
    file.flush().await?;
    drop(file);

    println!("Uploading probe file to iPhone...");
    let remote = afc
        .upload_file(&local, &name, |sent, total| {
            println!("  {sent}/{total} bytes");
        })
        .await
        .context("Probe upload failed")?;

    let downloaded = afc.read_file(&remote).await.context("Probe readback failed")?;
    anyhow::ensure!(downloaded == payload, "probe file checksum mismatch");
    println!("Upload verified: {remote} ({} bytes)", downloaded.len());

    let url = file_url(&remote);
    let mut pasteboard = UrlPasteboard::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake)
        .await
        .context("Pasteboard service is unavailable")?;
    pasteboard
        .set_file_urls(&[url.clone()])
        .await
        .context("Could not publish file URL on the device pasteboard")?;
    println!("Published on iPhone pasteboard: {url}");
    println!("Open Files or Notes on the iPhone and paste to check whether the file URL is accepted.");

    afc.remove_file(&remote).await.ok();
    tokio::fs::remove_file(&local).await.ok();
    println!("Probe complete. Temporary files cleaned up.");
    Ok(())
}

/// Send real files through the same pipeline used by drag-and-drop.
async fn send_files(udid: Option<&str>, paths: Vec<std::path::PathBuf>) -> Result<()> {
    use iphone_mirror_rs::transfer::afc::AfcTransfer;

    let mut wifi_device = wifi::endpoint_from_env();
    if wifi_device.is_none() {
        wifi_device = wifi::WifiDiscovery::discover().await.ok().and_then(|devices| {
            devices
                .into_iter()
                .find(|device| udid.is_none() || device.identifier == udid.unwrap_or_default())
        });
    }
    let usb_device = if wifi_device.is_none() {
        Some(DeviceScanner::select_device(udid).await?)
    } else {
        None
    };
    let source = if let Some(wifi_device) = &wifi_device {
        TunnelSource::Wifi(wifi_device)
    } else {
        TunnelSource::Usb(usb_device.as_ref().expect("USB device selected"))
    };

    let mut tunnel = source.connect().await.context("CoreDevice/RSD connect failed")?;
    let afc = AfcTransfer::connect(&mut tunnel.adapter, &mut tunnel.handshake)
        .await
        .context("AFC file service is unavailable")?;
    let (paste_tx, _paste_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut manager =
        TransferManager::spawn(afc, &mut tunnel.adapter, &mut tunnel.handshake, paste_tx)
            .await
            .context("Could not initialize file transfer")?;
    let tx = manager.sender();
    tx.send(TransferCommand::SendFiles(paths))
        .context("Transfer queue closed")?;

    while let Some(event) = manager.next_event().await {
        match event {
            TransferEvent::Started { files } => println!("Sending {files} file(s)"),
            TransferEvent::Progress {
                name,
                sent,
                total,
                file_index,
                file_count,
            } => {
                let percent = if total == 0 { 100 } else { sent * 100 / total };
                println!("  [{file_index}/{file_count}] {name}: {percent}%");
            }
            TransferEvent::FileStaged { name, url, .. } => {
                println!("  Staged {name}: {url}");
            }
            TransferEvent::Published { files } => println!("Published {files} file URL(s) on iPhone"),
            TransferEvent::Completed { files } => {
                println!("Done: {files} file(s) sent");
                return Ok(());
            }
            TransferEvent::Failed { name, message } => {
                anyhow::bail!("transfer failed for {:?}: {}", name, message);
            }
        }
    }
    anyhow::bail!("transfer ended without a result")
}

async fn diagnose(udid: Option<&str>) -> Result<()> {
    let wifi_device = wifi::endpoint_from_env();
    let usb_device = if wifi_device.is_none() {
        Some(DeviceScanner::select_device(udid).await?)
    } else {
        None
    };
    let source = if let Some(wifi_device) = &wifi_device {
        println!("Device: {} (Wi-Fi)", wifi_device.identifier);
        TunnelSource::Wifi(wifi_device)
    } else {
        let device = usb_device.as_ref().expect("USB device selected above");
        println!("Device: {} ({})", device.udid, device.connection_type);
        TunnelSource::Usb(device)
    };

    let tunnel = source.connect().await.context("CoreDevice/RSD diagnostics failed")?;

    println!("RSD: connected ({} services)", tunnel.services.len());
    println!(
        "Display service: {}",
        if tunnel.has_display_service() {
            "available"
        } else {
            "not available (Developer Mode or Developer Disk Image may be required)"
        }
    );

    if env::var_os("IPHONE_MIRROR_LIST_SERVICES").is_some() {
        for service in tunnel.sorted_services() {
            println!("  {service}");
        }
    }

    Ok(())
}

/// Run each CoreDevice session in a fresh process. A cable disconnect tears
/// down several private-protocol clients and native decoder/output objects at
/// once; recreating the worker process guarantees that no stale RSD, HID,
/// AudioToolbox, or VideoToolbox state leaks into the next attempt.
async fn supervise_mirror() -> Result<()> {
    const WORKER_ENV: &str = "IPHONE_MIRROR_SESSION_WORKER";
    if env::var_os(WORKER_ENV).is_some() {
        return run_mirror().await;
    }

    let executable = env::current_exe().context("Could not locate the iphone-mirror executable")?;
    let mut settings = load_settings();
    let mut attempt = 0_u64;
    loop {
        attempt += 1;
        if attempt > 1 {
            info!(attempt, "Attempting to reconnect the iPhone mirror session");
        }

        let mut child = tokio::process::Command::new(&executable)
            .arg("mirror")
            .env(WORKER_ENV, "1")
            .env(
                "IPHONE_MIRROR_TRANSPORT",
                match settings.transport {
                    TransportPreference::Auto => "auto",
                    TransportPreference::Usb => "usb",
                    TransportPreference::Wifi => "wifi",
                    TransportPreference::Airplay => "airplay",
                },
            )
            .env(
                "IPHONE_MIRROR_AUTO_ROTATE",
                if settings.auto_rotate { "1" } else { "0" },
            )
            .kill_on_drop(true)
            .spawn()
            .context("Failed to launch the mirror session worker")?;

        let status = tokio::select! {
            status = child.wait() => status.context("Mirror session worker wait failed")?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("Failed to listen for Ctrl-C")?;
                let _ = child.kill().await;
                return Ok(());
            }
        };
        if status.success() {
            return Ok(());
        }

        warn!(
            ?status,
            "Mirror session ended; showing the connection launcher"
        );
        match launcher_alert(
            "No iPhone Connection",
            &format!(
                "iPhone Mirror could not start over {}.\n\nConnect and trust the iPhone. For Wi-Fi, enable ‘Show this iPhone when on Wi-Fi’ in Finder first.",
                settings.transport.label()
            ),
        ) {
            LauncherAction::Retry => {}
            LauncherAction::Settings => {
                edit_settings(&mut settings)?;
            }
            LauncherAction::Quit => return Ok(()),
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            signal = tokio::signal::ctrl_c() => {
                signal.context("Failed to listen for Ctrl-C")?;
                return Ok(());
            }
        }
    }
}

/// Where the RSD tunnel comes from. USB goes through usbmuxd; Wi-Fi uses a
/// RemotePairing TLS-PSK tunnel and already provides an adapter/handshake.
enum TunnelSource<'a> {
    Usb(&'a ConnectedDevice),
    Wifi(&'a WifiDevice),
}

impl TunnelSource<'_> {
    async fn connect(&self) -> Result<RsdTunnel> {
        match self {
            Self::Usb(device) => RsdTunnel::connect(Box::new(device.provider())).await,
            Self::Wifi(device) => {
                let (adapter, handshake) = wifi::connect(device).await?;
                Ok(RsdTunnel::from_parts(adapter, handshake))
            }
        }
    }
}

/// Connect to the device's RSD services, repairing the DeveloperDiskImage when
/// the display service is missing from the catalog. Installing the DDI changes
/// the advertised service list, so a fresh tunnel is opened to pick it up.
async fn connect_tunnel_with_ddi(source: &TunnelSource<'_>) -> Result<RsdTunnel> {
    let mut tunnel = source.connect().await?;
    if tunnel.has_display_service() {
        return Ok(tunnel);
    }

    warn!("Display service missing from RSD catalog; attempting automatic DDI repair");
    let installed = DdiMounter::ensure_display_service(&mut tunnel.adapter, &mut tunnel.handshake)
        .await
        .context("Automatic DeveloperDiskImage repair failed")?;
    if !installed {
        anyhow::bail!(
            "Display service is unavailable and no compatible DeveloperDiskImage could be \
             prepared. Check your internet connection (needed to fetch the DDI) and that \
             Developer Mode is enabled."
        );
    }

    for attempt in 1..=4 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tunnel = source.connect().await?;
        if tunnel.has_display_service() {
            info!(attempt, "Display service available after DDI install");
            return Ok(tunnel);
        }
        warn!(attempt, "Display service still absent; waiting for DDI daemons");
    }

    anyhow::bail!(
        "DeveloperDiskImage was installed but the display service is still not advertised. \
         Unlock the iPhone and reconnect it once to let the developer daemons load."
    )
}

async fn run_mirror() -> Result<()> {
    info!("Starting iphone-mirror-rs (Native 60fps macOS/Hackintosh Mirror)...");

    let settings = AppSettings {
        transport: match env::var("IPHONE_MIRROR_TRANSPORT").as_deref() {
            Ok("usb") => TransportPreference::Usb,
            Ok("wifi") => TransportPreference::Wifi,
            Ok("airplay") => TransportPreference::Airplay,
            _ => TransportPreference::Auto,
        },
        auto_rotate: env::var("IPHONE_MIRROR_AUTO_ROTATE").as_deref() != Ok("0"),
    };
    if settings.transport == TransportPreference::Airplay {
        return run_airplay_mirror(settings).await;
    }
    let selected_udid = env::var("IPHONE_MIRROR_UDID").ok();
    let wifi_device = wifi::endpoint_from_env();
    let usb_device = if wifi_device.is_none() {
        Some(
            DeviceScanner::select_device_with_transport(
                selected_udid.as_deref(),
                settings.transport,
            )
            .await?,
        )
    } else {
        None
    };
    let source = if let Some(wifi_device) = &wifi_device {
        info!(host = %wifi_device.host, port = wifi_device.port, name = %wifi_device.name, "Wi-Fi iPhone selected");
        TunnelSource::Wifi(wifi_device)
    } else {
        let device = usb_device.as_ref().expect("USB device selected above");
        info!(udid = %device.udid, conn = %device.connection_type, "iPhone detected");
        TunnelSource::Usb(device)
    };

    let mut tunnel = connect_tunnel_with_ddi(&source).await?;
    let mut identity_keys = Vec::new();
    if let Some(w) = &wifi_device {
        identity_keys.push(w.identifier.clone());
        identity_keys.push(w.host.to_ascii_lowercase());
    }
    if let Some(u) = &usb_device {
        identity_keys.push(u.udid.clone());
    }
    if let Some(udid) = &selected_udid {
        identity_keys.push(udid.clone());
    }
    query_and_cache_rsd_identity(&mut tunnel, &identity_keys).await;
    if !tunnel.has_display_service() {
        anyhow::bail!(
            "Display service is unavailable. Enable Developer Mode and ensure the developer image is mounted."
        );
    }

    let mut client = DisplayClient::start(&mut tunnel, 1)
        .await
        .context("Failed to start the display service stream")?;
    info!("Stream session negotiated; awaiting H.265 RTP packets");
    let mut hid = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        UniversalHidServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    .context("Timed out connecting to the universal HID service")?
    .context("Failed to connect to the universal HID service")?;
    info!("Universal HID service connected");
    let mut indigo_hid = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        IndigoHidClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    .context("Timed out connecting to the Indigo HID service")?
    .context("Failed to connect to the Indigo HID service")?;
    info!("Indigo HID service connected");
    let mut keyboard = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        hid.create_main_keyboard(),
    )
    .await
    .context("Timed out creating the iPhone keyboard surface")?
    .map_err(|error| anyhow::anyhow!("Failed to create the iPhone keyboard surface: {error}"))?;
    info!("Virtual keyboard surface created");
    let mut clipboard_writer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        PasteboardServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    .context("Timed out connecting to the pasteboard service")?
    .context("Failed to connect to the pasteboard service")?;
    let mut clipboard_watcher = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        PasteboardServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    .context("Timed out opening the pasteboard notification channel")?
    .context("Failed to open the pasteboard notification channel")?;
    clipboard_watcher
        .set_change_notifications(
            true,
            GENERAL_PASTEBOARD,
            Some(DataInclusionPolicy::AllResolved),
        )
        .await
        .context("Failed to subscribe to iPhone clipboard changes")?;
    info!("Pasteboard services connected");

    // Separate HID connection used only for the Cmd+V sent after a file drop.
    let paste_hid = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        IndigoHidClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    .context("Timed out connecting the auto-paste HID channel")?
    .context("Failed to connect the auto-paste HID channel")?;

    let springboard = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        SpringBoardServicesClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    {
        Ok(Ok(client)) => {
            info!("SpringBoard services connected for interface orientation");
            Some(client)
        }
        Ok(Err(error)) => {
            warn!(%error, "SpringBoard services unavailable; auto-rotate will rely on Ctrl+R");
            None
        }
        Err(_) => {
            warn!("Timed out connecting to SpringBoard services; auto-rotate will rely on Ctrl+R");
            None
        }
    };

    let event_loop = winit::event_loop::EventLoop::<UiEvent>::with_user_event()
        .build()
        .context("Failed to create the macOS event loop")?;
    info!("Mirror window event loop created");
    let proxy = event_loop.create_proxy();
    let stream_proxy = proxy.clone();

    // File-transfer pipeline. AFC and the pasteboard writer are independent
    // tunnel connections, so the media task can keep owning the main tunnel.
    let (paste_tx, mut paste_rx) = tokio::sync::mpsc::unbounded_channel();
    let transfer_command_tx =
        match AfcTransfer::connect(&mut tunnel.adapter, &mut tunnel.handshake).await {
            Ok(afc) => {
                match TransferManager::spawn(afc, &mut tunnel.adapter, &mut tunnel.handshake, paste_tx).await {
                    Ok(mut manager) => {
                        let tx = manager.sender();
                        let transfer_proxy = proxy.clone();
                        tokio::spawn(async move {
                            while let Some(event) = manager.next_event().await {
                                if transfer_proxy.send_event(UiEvent::Transfer(event)).is_err() {
                                    break;
                                }
                            }
                        });
                        info!("File transfer ready");
                        Some(tx)
                    }
                    Err(error) => {
                        warn!(%error, "Could not initialize the file transfer manager");
                        None
                    }
                }
            }
            Err(error) => {
                warn!(%error, "AFC file service unavailable; drag-and-drop disabled");
                None
            }
        };

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(3);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (button_tx, mut button_rx) = tokio::sync::mpsc::unbounded_channel::<HardwareButton>();
    let (clipboard_tx, mut clipboard_rx) =
        tokio::sync::mpsc::unbounded_channel::<ClipboardContent>();
    let audio_muted = Arc::new(AtomicBool::new(false));
    let audio_muted_stream = audio_muted.clone();

    if let Some(mut springboard) = springboard {
        let orientation_proxy = stream_proxy.clone();
        let mut orientation_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(500));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_rotation: Option<DisplayRotation> = None;
            loop {
                tokio::select! {
                    _ = poll.tick() => {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            springboard.get_interface_orientation(),
                        )
                        .await
                        {
                            Ok(Ok(orientation)) => {
                                let rotation = match orientation {
                                    InterfaceOrientation::Portrait => Some(DisplayRotation::Portrait),
                                    InterfaceOrientation::PortraitUpsideDown => {
                                        Some(DisplayRotation::PortraitUpsideDown)
                                    }
                                    InterfaceOrientation::LandscapeRight => {
                                        Some(DisplayRotation::LandscapeRight)
                                    }
                                    InterfaceOrientation::LandscapeLeft => {
                                        Some(DisplayRotation::LandscapeLeft)
                                    }
                                    InterfaceOrientation::Unknown => None,
                                };
                                if let Some(rotation) = rotation {
                                    if last_rotation != Some(rotation) {
                                        last_rotation = Some(rotation);
                                        if orientation_proxy
                                            .send_event(UiEvent::Orientation(rotation))
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(Err(error)) => {
                                debug!(%error, "SpringBoard orientation query stopped");
                                break;
                            }
                            Err(_) => {
                                debug!("SpringBoard orientation query timed out");
                            }
                        }
                    }
                    changed = orientation_shutdown.changed() => {
                        if changed.is_err() || *orientation_shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }

    let signal_proxy = stream_proxy.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(signal) => signal,
                    Err(error) => {
                        warn!(%error, "Could not install the graceful SIGTERM handler");
                        return;
                    }
                };
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            let _ = signal_proxy.send_event(UiEvent::ShutdownRequested);
        }
    });

    tokio::spawn(async move {
        while let Some(button) = button_rx.recv().await {
            let (usage_page, usage_code) = button.usage();
            let press = indigo_hid
                .send_button(usage_page, usage_code, ButtonState::Down)
                .await;
            if let Err(error) = press {
                warn!(%error, button = button.label(), "Failed to press hardware button");
                continue;
            }
            tokio::time::sleep(button.hold()).await;
            if let Err(error) = indigo_hid
                .send_button(usage_page, usage_code, ButtonState::Up)
                .await
            {
                warn!(%error, button = button.label(), "Failed to release hardware button");
            } else {
                info!(button = button.label(), "Sent hardware button");
            }
        }
    });

    let clipboard_proxy = stream_proxy.clone();
    let mut clipboard_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                snapshot = clipboard_watcher.recv_push() => match snapshot {
                    Ok(snapshot) => {
                        let content = snapshot
                            .image()
                            .filter(|(_, bytes)| bytes.len() <= MAX_CLIPBOARD_IMAGE_BYTES)
                            .map(|(uti, bytes)| ClipboardContent::Image {
                                uti: uti.to_string(),
                                bytes,
                            })
                            .or_else(|| snapshot.text().map(ClipboardContent::Text));
                        if let Some(content) = content {
                            if clipboard_proxy.send_event(UiEvent::Clipboard(content)).is_err() {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        debug!(%error, "iPhone clipboard notification stream ended");
                        break;
                    }
                },
                changed = clipboard_shutdown.changed() => {
                    if changed.is_err() || *clipboard_shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });

    let latest_frame: Arc<std::sync::Mutex<Option<DecodedFrame>>> =
        Arc::new(std::sync::Mutex::new(None));
    let frame_pending = Arc::new(AtomicBool::new(false));
    let latest_frame_tx = latest_frame.clone();
    let frame_pending_tx = frame_pending.clone();
    tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if let Ok(mut slot) = latest_frame_tx.lock() {
                *slot = Some(frame);
            }
            if !frame_pending_tx.swap(true, Ordering::AcqRel)
                && proxy.send_event(UiEvent::FrameReady).is_err()
            {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let result: Result<()> = async {
            let mut tunnel = tunnel;
            let mut paste_hid = paste_hid;
            let mut depacketizer = H265Depacketizer::new();
            let mut parameter_sets = ParameterSetCache::new();
            let mut decoder: Option<HardwareDecoder> = None;
            let mut decoder_sets: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = None;
            let decoded_frames = Arc::new(AtomicU64::new(0));
            let mut frames_at_check = 0_u64;
            let mut last_nal_at_check = 0_u64;
            let mut decoder_stalled_ticks = 0_u32;
            let mut decoder_check = tokio::time::interval(std::time::Duration::from_secs(1));
            decoder_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut decode_failures = 0_u32;
            let mut nal_count = 0_u64;
            let mut audio_packet_count = 0_u64;
            let mut video_highest_sequence = 0_u32;
            let mut last_pli = std::time::Instant::now();
            let mut audio_highest_sequence = 0_u32;
            let mut audio_decode_errors = 0_u32;
            let mut audio_decoder = AacEldDecoder::new()?;
            // 300 ms at 48 kHz = 14,400 stereo sample-pairs.
            let (mut audio_output, audio_ring, audio_mute) = create_output(14_400);
            let audio_backend_name = audio_output.backend_name();
            audio_output.prepare()?;
            audio_output.start()?;
            info!(backend = audio_backend_name, "Audio output started");
            let telemetry = Arc::new(AudioTelemetry::new());
            let mut audio_jitter = AudioJitter::new(8_000); // ~167 ms initial target
            let mut audio_reorder = AudioReorder::new(8, std::time::Duration::from_millis(25));
            let mut audio_rtcp_stats = RtcpReportStats::new();
            let mut video_rtcp_stats = RtcpReportStats::new();
            let mut ring_underflows_at_check = 0_u64;
            let mut audio_was_muted = false;
            let mut audio_returned_at_check = 0_u64;
            let mut audio_stalled_ticks = 0_u32;
            let mut last_audio_restart: Option<std::time::Instant> = None;
            let mut audio_output_device = audio_output.current_output_device().unwrap_or(0);
            let mut audio_rtcp = tokio::time::interval(std::time::Duration::from_secs(1));
            audio_rtcp.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut audio_diag = tokio::time::interval(std::time::Duration::from_secs(5));
            audio_diag.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut audio_peak = 0_u16;
            let mut audio_pcm_bytes = 0_u64;
            let audio_stats_start = std::time::Instant::now();
            let mut audio_dump = env::var_os("IPHONE_MIRROR_AUDIO_DUMP").map(|directory| {
                let directory = std::path::PathBuf::from(directory);
                (
                    std::fs::File::create(directory.join("pcm.raw")).expect("create pcm dump"),
                    std::fs::File::create(directory.join("au.bin")).expect("create AU dump"),
                )
            });
            let mut scroll_position: Option<(u16, u16)> = None;

            loop {
                tokio::select! {
                    packet = client.recv_video_packet() => {
                        let packet = packet?;
                        if RtpPacket::is_rtcp(&packet) {
                            continue;
                        }
                        match RtpPacket::parse(&packet) {
                            Ok(rtp) => {
                            let seq = rtp.sequence_number as u32;
                            let cycles = video_highest_sequence >> 16;
                            let last = video_highest_sequence & 0xFFFF;
                            let cycles = if seq < last && last - seq > 0x8000 { cycles + 1 } else { cycles };
                            let extended = (cycles << 16) | seq;
                            if video_highest_sequence == 0
                                || extended.wrapping_sub(video_highest_sequence) < 0x8000_0000
                            {
                                video_highest_sequence = extended;
                            }
                            match depacketizer.process_packet(&rtp) {
                            Ok(nals) => for nal in nals {
                                nal_count += 1;
                                if matches!(
                                    ParameterSetCache::parse_nal_type(&nal),
                                    Some(
                                        NalUnitType::VpsNut
                                            | NalUnitType::SpsNut
                                            | NalUnitType::PpsNut
                                    )
                                ) {
                                    parameter_sets.inspect_and_cache(&nal);
                                    continue;
                                }

                                // iOS can change VPS/SPS/PPS mid-stream (rotation,
                                // resolution change, encoder reset). Wait until a
                                // non-parameter-set NAL arrives so all three sets are
                                // updated before rebuilding the decoder once.
                                let current_sets = match (
                                    parameter_sets.vps.as_ref(),
                                    parameter_sets.sps.as_ref(),
                                    parameter_sets.pps.as_ref(),
                                ) {
                                    (Some(vps), Some(sps), Some(pps)) => {
                                        Some((vps.clone(), sps.clone(), pps.clone()))
                                    }
                                    _ => None,
                                };
                                if let Some(sets) = current_sets {
                                    if decoder_sets.as_ref() != Some(&sets) {
                                        let format = FormatDescription::from_hevc_parameter_sets(
                                            &sets.0, &sets.1, &sets.2,
                                        )?;
                                        decoder = Some(HardwareDecoder::new(format, frame_tx.clone(), decoded_frames.clone())?);
                                        decoder_sets = Some(sets);
                                        decode_failures = 0;
                                        info!(nal_count, "VideoToolbox decoder ready for current HEVC parameter sets");
                                    }
                                }

                                let mut recreate_decoder = false;
                                if let Some(decoder) = decoder.as_mut() {
                                    if let Err(error) = decoder.decode_nal(&nal) {
                                        decode_failures += 1;
                                        if decode_failures == 1 || decode_failures % 100 == 0 {
                                            debug!(%error, decode_failures, "VideoToolbox rejected a NAL unit");
                                        }
                                        // A sustained run of rejections means the
                                        // decoder no longer matches the stream even
                                        // if the parameter sets looked unchanged.
                                        if decode_failures >= 500 {
                                            recreate_decoder = true;
                                        }
                                    } else {
                                        decode_failures = 0;
                                    }
                                }
                                if recreate_decoder {
                                    if let Some(sets) = decoder_sets.take() {
                                        warn!(decode_failures, "Recreating VideoToolbox decoder after repeated rejections");
                                        let format = FormatDescription::from_hevc_parameter_sets(
                                            &sets.0, &sets.1, &sets.2,
                                        )?;
                                        decoder = Some(HardwareDecoder::new(format, frame_tx.clone(), decoded_frames.clone())?);
                                        decoder_sets = Some(sets);
                                        decode_failures = 0;
                                    }
                                }
                                if nal_count % 300 == 0 {
                                    info!(nal_count, "Receiving active HEVC stream");
                                }
                            },
                            Err(error) => debug!(%error, "Dropped malformed video NAL"),
                            }

                            if depacketizer.last_gap >= 100 {
                                // A burst this large almost certainly swallowed
                                // VPS/SPS/PPS or a whole IDR. The decoder cannot
                                // recover on its own, so renegotiate immediately
                                // instead of waiting for a multi-second stall.
                                warn!(
                                    gap = depacketizer.last_gap,
                                    "Large RTP loss; restarting media session"
                                );
                                client.restart(&mut tunnel, 1).await?;
                                depacketizer = H265Depacketizer::new();
                                parameter_sets = ParameterSetCache::new();
                                decoder = None;
                                decoder_sets = None;
                                video_highest_sequence = 0;
                                audio_highest_sequence = 0;
                                // A renegotiated stream starts with fresh SSRCs and
                                // sequence numbers. Stale reorder/stats state would
                                // discard every new packet as out-of-window.
                                audio_reorder.reset();
                                audio_rtcp_stats.reset();
                                video_rtcp_stats.reset();
                                audio_jitter = AudioJitter::new(8_000);
                                audio_decode_errors = 0;
                                ring_underflows_at_check = 0;
                                audio_ring.clear();
                                audio_output
                                    .restart(AudioRestartReason::MediaSessionRestarted)?;
                                decoder_stalled_ticks = 0;
                                frames_at_check = decoded_frames.load(Ordering::Relaxed);
                                last_nal_at_check = nal_count;
                                info!("Media session restarted after RTP loss burst");
                            } else if depacketizer.sequence_gap {
                                depacketizer.sequence_gap = false;
                                if last_pli.elapsed() >= std::time::Duration::from_millis(500) {
                                    if let Some(pli) = client.video_pli() {
                                        match client.send_video_rtcp(pli).await {
                                            Ok(()) => {
                                                info!("Sent RTCP PLI after video packet loss");
                                                last_pli = std::time::Instant::now();
                                            }
                                            Err(error) => {
                                                debug!(%error, "Failed to send video PLI")
                                            }
                                        }
                                    }
                                }
                            }
                            }
                            Err(error) => debug!(%error, "Dropped malformed media packet"),
                        }
                    }
                    packet = client.recv_audio_packet() => {
                        let packet = packet?;
                        if RtpPacket::is_rtcp(&packet) {
                            continue;
                        }
                        let packet = RtpPacket::parse(&packet)?;
                        if packet.payload_type != 101 || packet.payload.is_empty() {
                            continue;
                        }
                        audio_packet_count += 1;
                        telemetry.rtp_packets.fetch_add(1, Ordering::Relaxed);
                        telemetry
                            .last_rtp_ms
                            .store(now_millis(), Ordering::Relaxed);
                        let sequence = packet.sequence_number as u32;
                        let cycles = audio_highest_sequence >> 16;
                        let last_sequence = audio_highest_sequence & 0xFFFF;
                        let cycles = if sequence < last_sequence
                            && last_sequence - sequence > 0x8000
                        {
                            cycles + 1
                        } else {
                            cycles
                        };
                        let extended = (cycles << 16) | sequence;
                        if audio_highest_sequence == 0
                            || extended.wrapping_sub(audio_highest_sequence) < 0x8000_0000
                        {
                            audio_highest_sequence = extended;
                        }
                        audio_jitter.observe(extended);
                        audio_rtcp_stats.observe(extended, packet.timestamp);
                        let payload_type = packet.payload_type;
                        audio_reorder.push(AudioPacket {
                            sequence: extended,
                            timestamp: packet.timestamp,
                            payload: packet.payload.clone(),
                        });

                        for reordered in audio_reorder.pop_ready() {
                            let pcm = match reordered {
                                ReorderedAudio::Silence => vec![0_u8; 1920],
                                ReorderedAudio::Packet(packet) => {
                                    if let Some((_, au_dump)) = audio_dump.as_mut() {
                                        let _ = au_dump.write_all(&(packet.payload.len() as u32).to_le_bytes());
                                        let _ = au_dump.write_all(&packet.payload);
                                    }
                                    match audio_decoder.decode(&packet.payload) {
                                        Ok(pcm) => {
                                            audio_decode_errors = 0;
                                            pcm
                                        }
                                        Err(error) => {
                                            audio_decode_errors += 1;
                                            telemetry.decode_errors.fetch_add(1, Ordering::Relaxed);
                                            if audio_decode_errors == 1 {
                                                debug!(%error, payload_type, "AAC-ELD decode failed");
                                            }
                                            if audio_decode_errors >= 5 {
                                                match AacEldDecoder::new() {
                                                    Ok(decoder) => {
                                                        audio_decoder = decoder;
                                                        audio_decode_errors = 0;
                                                        audio_reorder.reset();
                                                        info!("Recreated AAC decoder after repeated failures");
                                                    }
                                                    Err(error) => warn!(%error, "Failed to recreate AAC decoder"),
                                                }
                                            }
                                            vec![0_u8; 1920]
                                        }
                                    }
                                }
                            };
                            telemetry.decode_ok.fetch_add(1, Ordering::Relaxed);
                            if let Some((pcm_dump, _)) = audio_dump.as_mut() {
                                let _ = pcm_dump.write_all(&pcm);
                            }
                            audio_pcm_bytes += pcm.len() as u64;
                            let samples: Vec<i16> = pcm
                                .chunks_exact(2)
                                .map(|s| i16::from_le_bytes([s[0], s[1]]))
                                .collect();
                            audio_peak = audio_peak
                                .max(samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0));
                            let written = audio_ring.write(&samples);
                            telemetry
                                .frames_written
                                .fetch_add((written / 2) as u64, Ordering::Relaxed);
                            telemetry
                                .last_pcm_ms
                                .store(now_millis(), Ordering::Relaxed);
                        }
                        if audio_packet_count % 500 == 0 {
                            let elapsed = audio_stats_start.elapsed().as_secs_f64();
                            info!(
                                audio_packet_count,
                                packets_per_second = audio_packet_count as f64 / elapsed,
                                peak = audio_peak,
                                pcm_bytes = audio_pcm_bytes,
                                jitter_depth = audio_ring.frames_available(),
                                "Playing iPhone system audio"
                            );
                        }
                    }
                    _ = audio_rtcp.tick() => {
                        audio_rtcp_stats.refresh();
                        audio_rtcp_stats.refresh_dlsr();
                        video_rtcp_stats.refresh();
                        video_rtcp_stats.refresh_dlsr();
                        if let Some(report) = client.audio_rtcp_report(&audio_rtcp_stats) {
                            if let Err(error) = client.send_audio_rtcp(report).await {
                                debug!(%error, "Failed to send the audio RTCP keepalive");
                            }
                        }
                        if let Some(report) = client.video_rtcp_report(&video_rtcp_stats) {
                            if let Err(error) = client.send_video_rtcp(report).await {
                                debug!(%error, "Failed to send the video RTCP keepalive");
                            }
                        }

                        // Mute changes are driven by the UI atomic; the backend
                        // keeps its output alive and renders silence internally.
                        let muted = audio_muted_stream.load(Ordering::Relaxed);
                        if muted != audio_was_muted {
                            audio_mute.set(muted);
                            if !muted {
                                // Never replay PCM captured while muted.
                                audio_ring.clear();
                            }
                            audio_was_muted = muted;
                            debug!(muted, "Audio mute state updated");
                        }

                        // A render callback that stops while RTP keeps arriving
                        // means the output backend itself is wedged.
                        let stats = audio_output.frame_stats();
                        if stats.callbacks > audio_returned_at_check {
                            audio_returned_at_check = stats.callbacks;
                            audio_stalled_ticks = 0;
                        } else if audio_stalled_ticks < 3 {
                            audio_stalled_ticks += 1;
                        }
                        // Default output device may change (HDMI, USB, Bluetooth,
                        // or a driver reset). Opt-in because some drivers report
                        // transient IDs that would cause needless restarts.
                        let current_device = if device_switch_enabled() {
                            audio_output.current_output_device()
                        } else {
                            None
                        };
                        if let Some(device) = current_device {
                            if audio_output_device != 0 && device != audio_output_device {
                                let since = last_audio_restart
                                    .map(|t| t.elapsed())
                                    .unwrap_or(std::time::Duration::from_secs(3600));
                                if since >= std::time::Duration::from_secs(2) {
                                    warn!(old = audio_output_device, new = device, "Audio output device changed");
                                    audio_ring.clear();
                                    audio_output.restart(AudioRestartReason::OutputDeviceChanged)?;
                                    audio_output_device = device;
                                    last_audio_restart = Some(std::time::Instant::now());
                                    audio_returned_at_check = audio_output.frame_stats().callbacks;
                                }
                            } else {
                                audio_output_device = device;
                            }
                        }

                        if audio_stalled_ticks >= 3
                            && telemetry.last_rtp_ms.load(Ordering::Relaxed) + 2000
                                > now_millis()
                        {
                            let since = last_audio_restart
                                .map(|t| t.elapsed())
                                .unwrap_or(std::time::Duration::from_secs(3600));
                            if since >= std::time::Duration::from_secs(2) {
                                warn!("Audio render callback stopped; recreating backend");
                                audio_output.restart(AudioRestartReason::RenderCallbackStalled)?;
                                last_audio_restart = Some(std::time::Instant::now());
                                audio_returned_at_check = audio_output.frame_stats().callbacks;
                                audio_stalled_ticks = 0;
                            }
                        }
                    }
                    _ = audio_diag.tick() => {
                        let stats = audio_output.frame_stats();
                        if stats.underruns > ring_underflows_at_check {
                            ring_underflows_at_check = stats.underruns;
                            audio_jitter.on_underflow();
                        } else {
                            audio_jitter.tick_stable();
                        }
                        telemetry
                            .callbacks
                            .store(stats.callbacks, Ordering::Relaxed);
                        telemetry
                            .silence_frames
                            .store(stats.silence_frames, Ordering::Relaxed);
                        telemetry
                            .underruns
                            .store(stats.underruns, Ordering::Relaxed);
                        telemetry
                            .overruns
                            .store(stats.overruns, Ordering::Relaxed);
                        if diagnostics_enabled() {
                            info!(
                                backend = stats.backend,
                                rtp = telemetry.rtp_packets.load(Ordering::Relaxed),
                                decode_errors = telemetry.decode_errors.load(Ordering::Relaxed),
                                ring_depth_ms = stats.frames_available / 48,
                                target_ms = audio_jitter.stats().target_frames / 48,
                                rtp_lost = audio_jitter.stats().lost,
                                underruns = stats.underruns,
                                overruns = stats.overruns,
                                callbacks = stats.callbacks,
                                silence_frames = stats.silence_frames,
                                restarts = stats.restarts,
                                "audio diagnostics"
                            );
                        }
                    }
                    _ = decoder_check.tick() => {
                        let frames = decoded_frames.load(Ordering::Relaxed);
                        if frames > frames_at_check {
                            frames_at_check = frames;
                            last_nal_at_check = nal_count;
                            decoder_stalled_ticks = 0;
                        } else {
                            decoder_stalled_ticks += 1;
                        }
                        if decoder_stalled_ticks >= 3
                            && nal_count > last_nal_at_check + 30
                            && decoder_sets.is_some()
                        {
                            // RTP keeps arriving but VideoToolbox has stopped
                            // emitting frames (e.g. after a burst of lost
                            // reference frames). A fresh session recovers it.
                            warn!(
                                nal_count,
                                frames, "Video decoder stopped producing frames; restarting media session"
                            );
                            client.restart(&mut tunnel, 1).await?;
                            depacketizer = H265Depacketizer::new();
                            parameter_sets = ParameterSetCache::new();
                            decoder = None;
                            decoder_sets = None;
                            video_highest_sequence = 0;
                            audio_highest_sequence = 0;
                            audio_reorder.reset();
                            audio_rtcp_stats.reset();
                            video_rtcp_stats.reset();
                            audio_jitter = AudioJitter::new(8_000);
                            audio_decode_errors = 0;
                            ring_underflows_at_check = 0;
                            audio_ring.clear();
                            audio_output
                                .restart(AudioRestartReason::MediaSessionRestarted)?;
                            decode_failures = 0;
                            decoder_stalled_ticks = 0;
                            frames_at_check = frames;
                            last_nal_at_check = nal_count;
                            info!("Media session restarted after video stall");
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    Some(()) = paste_rx.recv() => {
                        // A dropped file was published on the device pasteboard.
                        // Send Cmd+V as discrete Indigo keyboard events: the
                        // modifier must go down before V and come up after it,
                        // otherwise iOS only sees a bare "v".
                        {
                                let sequence: [(u64, ButtonState); 4] = [
                                    (0xE3, ButtonState::Down), // Left GUI / Command
                                    (0x19, ButtonState::Down), // V
                                    (0x19, ButtonState::Up),
                                    (0xE3, ButtonState::Up),
                                ];
                                let mut ok = true;
                                for (usage, state) in sequence {
                                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                                    if paste_hid.send_keyboard(usage, state).await.is_err() {
                                        ok = false;
                                        break;
                                    }
                                }
                                if ok {
                                    info!("Sent Cmd+V after file drop");
                                } else {
                                    warn!("Could not send Cmd+V after file drop");
                                }
                        }
                    }
                    Some(command) = input_rx.recv() => {
                        dispatch_hid_command(&mut hid, &mut keyboard, &mut scroll_position, command).await;
                    }
                    Some(content) = clipboard_rx.recv() => {
                        match content {
                            ClipboardContent::Text(text) => {
                                if let Err(error) = clipboard_writer.set_text(&text, GENERAL_PASTEBOARD).await {
                                    warn!(%error, "Failed to copy macOS clipboard text to iPhone");
                                } else {
                                    info!(bytes = text.len(), "Copied macOS clipboard text to iPhone");
                                }
                            }
                            ClipboardContent::Image { uti, bytes } => {
                                if let Err(error) = clipboard_writer.set_image(&bytes, &uti, GENERAL_PASTEBOARD).await {
                                    warn!(%error, %uti, "Failed to copy macOS clipboard image to iPhone");
                                } else {
                                    info!(%uti, bytes = bytes.len(), "Copied macOS clipboard image to iPhone");
                                }
                            }
                        }
                    }
                }
            }
            match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                hid.remove_main_keyboard(&mut keyboard),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(%error, "Failed to remove the virtual keyboard service"),
                Err(_) => warn!("Timed out removing the virtual keyboard service"),
            }
            match tokio::time::timeout(std::time::Duration::from_secs(5), client.stop()).await {
                Ok(Ok(())) => info!("Display stream stop acknowledged by iPhone"),
                Ok(Err(error)) => warn!(%error, "Display stream shutdown was not acknowledged"),
                Err(_) => warn!("Timed out waiting for display stream shutdown acknowledgement"),
            }
            Ok(())
        }.await;
        if let Err(error) = result {
            let _ = stream_proxy.send_event(UiEvent::StreamError(error.to_string()));
        } else {
            let _ = stream_proxy.send_event(UiEvent::ShutdownComplete);
        }
    });

    let mut app = MirrorApp {
        window: None,
        renderer: None,
        latest_frame,
        frame_pending,
        shutdown_tx,
        frame_count: 0,
        frame_size: None,
        cursor_position: None,
        touch_active: false,
        scroll_active: false,
        scroll_deadline: None,
        scroll_started: false,
        scroll_accum: (0.0, 0.0),
        scroll_anchor: (u16::MAX / 2, u16::MAX / 2),
        suppress_click_until: None,
        last_touch_position: None,
        input_tx,
        button_tx,
        audio_muted,
        screenshot_requested: false,
        always_on_top: false,
        fullscreen: false,
        auto_rotate: settings.auto_rotate,
        rotation: DisplayRotation::Portrait,
        manual_rotation_override: false,
        frame_is_landscape: None,
        pressed_keys: BTreeSet::new(),
        modifiers: winit::keyboard::ModifiersState::empty(),
        clipboard: SystemClipboard::new(),
        clipboard_tx,
        last_device_clipboard: None,
        transfer_tx: transfer_command_tx,
        drop_candidates: Vec::new(),
        drop_deadline: None,
        transfer_status: None,
        stream_error: None,
        shutting_down: false,
    };
    event_loop
        .run_app(&mut app)
        .context("macOS window event loop failed")?;
    if let Some(error) = app.stream_error {
        anyhow::bail!("Mirror session failed: {error}");
    }
    Ok(())
}

async fn dispatch_hid_command<R: idevice::ReadWrite>(
    hid: &mut UniversalHidServiceClient<R>,
    keyboard: &mut MainKeyboardService,
    scroll_position: &mut Option<(u16, u16)>,
    command: InputCommand,
) {
    debug!(?command, "Sending HID input event");
    let command_kind = match &command {
        InputCommand::Down(..) => "touch-down",
        InputCommand::Move(..) => "touch-move",
        InputCommand::Up(..) => "touch-up",
        InputCommand::ScrollStart(..) => "scroll-start",
        InputCommand::ScrollMove(..) => "scroll-move",
        InputCommand::ScrollEnd => "scroll-end",
        InputCommand::SetKeys(..) => "keyboard",
        InputCommand::NavigateBack(..) => "navigate-back",
    };
    let result = match command {
        InputCommand::Down(x, y) | InputCommand::Move(x, y) => {
            hid.send_touchscreen(idevice::core_device::TOUCHSCREEN_STATE_CONTACT, x, y, None)
                .await
        }
        InputCommand::Up(x, y) => {
            hid.send_touchscreen(idevice::core_device::TOUCHSCREEN_STATE_RELEASE, x, y, None)
                .await
        }
        InputCommand::ScrollStart(x, y) => {
            *scroll_position = Some((x, y));
            hid.send_touchscreen(idevice::core_device::TOUCHSCREEN_STATE_CONTACT, x, y, None)
                .await
        }
        InputCommand::ScrollMove(dx, dy) => {
            if let Some((x, y)) = *scroll_position {
                let (x, y) = offset_touch_position((x, y), (dx, dy));
                *scroll_position = Some((x, y));
                hid.send_touchscreen(idevice::core_device::TOUCHSCREEN_STATE_CONTACT, x, y, None)
                    .await
            } else {
                Ok(())
            }
        }
        InputCommand::ScrollEnd => {
            if let Some((x, y)) = scroll_position.take() {
                hid.send_touchscreen(idevice::core_device::TOUCHSCREEN_STATE_RELEASE, x, y, None)
                    .await
            } else {
                Ok(())
            }
        }
        InputCommand::NavigateBack(rotation) => {
            *scroll_position = None;
            let (sx, sy) = rotation.to_buffer_u16(128, 32_768);
            let (ex, ey) = rotation.to_buffer_u16(48_000, 32_768);
            hid.drag(sx, sy, ex, ey, 14, 12).await
        }
        InputCommand::SetKeys(usages) => {
            match usages
                .into_iter()
                .map(idevice::core_device::KeyboardUsage::new)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| idevice::IdeviceError::UnexpectedResponse(error.to_string()))
            {
                Ok(usages) => hid
                    .set_main_keyboard_usages(keyboard, usages)
                    .await
                    .map_err(|error| idevice::IdeviceError::UnexpectedResponse(error.to_string())),
                Err(err) => Err(err),
            }
        }
    };
    if let Err(error) = result {
        warn!(%error, command_kind, "Failed to send HID input event");
    }
}

async fn connect_optional_hid_tunnel() -> Result<RsdTunnel> {
    if let Some(wifi_device) = wifi::endpoint_from_env() {
        return TunnelSource::Wifi(&wifi_device).connect().await;
    }
    let udid = env::var("IPHONE_MIRROR_UDID").ok();
    let device = DeviceScanner::select_device_with_transport(
        udid.as_deref(),
        TransportPreference::Auto,
    )
    .await?;
    TunnelSource::Usb(&device).connect().await
}

async fn run_airplay_hybrid_hid(
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<InputCommand>,
    mut button_rx: tokio::sync::mpsc::UnboundedReceiver<HardwareButton>,
    mut clipboard_rx: tokio::sync::mpsc::UnboundedReceiver<ClipboardContent>,
    clipboard_proxy: winit::event_loop::EventLoopProxy<UiEvent>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let tunnel = match tokio::time::timeout(
        std::time::Duration::from_secs(4),
        connect_optional_hid_tunnel(),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(err)) => {
            info!("AirPlay running in view + audio mode (no paired CoreDevice HID: {err})");
            drain_airplay_inputs(&mut input_rx, &mut button_rx, &mut clipboard_rx, &mut shutdown_rx).await;
            return;
        }
        Err(_) => {
            info!("AirPlay running in view + audio mode (HID tunnel timed out)");
            drain_airplay_inputs(&mut input_rx, &mut button_rx, &mut clipboard_rx, &mut shutdown_rx).await;
            return;
        }
    };

    let mut tunnel = tunnel;
    let mut hid_keys = Vec::new();
    if let Ok(udid) = env::var("IPHONE_MIRROR_UDID") {
        hid_keys.push(udid);
    }
    if let Ok(host) = env::var("IPHONE_MIRROR_WIFI_HOST") {
        hid_keys.push(host.to_ascii_lowercase());
    }
    query_and_cache_rsd_identity(&mut tunnel, &hid_keys).await;
    let Ok(Ok(mut hid)) = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        UniversalHidServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    else {
        warn!("Hybrid HID unavailable over AirPlay; falling back to view + audio mode");
        drain_airplay_inputs(&mut input_rx, &mut button_rx, &mut clipboard_rx, &mut shutdown_rx).await;
        return;
    };

    let Ok(Ok(mut keyboard)) = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        hid.create_main_keyboard(),
    )
    .await
    else {
        warn!("Hybrid keyboard unavailable over AirPlay; falling back to view + audio mode");
        drain_airplay_inputs(&mut input_rx, &mut button_rx, &mut clipboard_rx, &mut shutdown_rx).await;
        return;
    };

    let mut indigo_hid = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        IndigoHidClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    .ok()
    .and_then(|r| r.ok());

    let mut clipboard_writer = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        PasteboardServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    .ok()
    .and_then(|r| r.ok());

    if let Ok(Ok(mut clipboard_watcher)) = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        PasteboardServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
    )
    .await
    {
        if clipboard_watcher
            .set_change_notifications(
                true,
                GENERAL_PASTEBOARD,
                Some(DataInclusionPolicy::AllResolved),
            )
            .await
            .is_ok()
        {
            let mut cw_shutdown = shutdown_rx.clone();
            let cw_proxy = clipboard_proxy.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        snapshot = clipboard_watcher.recv_push() => match snapshot {
                            Ok(snapshot) => {
                                let content = snapshot
                                    .image()
                                    .filter(|(_, bytes)| bytes.len() <= MAX_CLIPBOARD_IMAGE_BYTES)
                                    .map(|(uti, bytes)| ClipboardContent::Image {
                                        uti: uti.to_string(),
                                        bytes,
                                    })
                                    .or_else(|| snapshot.text().map(ClipboardContent::Text));
                                if let Some(content) = content {
                                    if cw_proxy.send_event(UiEvent::Clipboard(content)).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(_) => break,
                        },
                        changed = cw_shutdown.changed() => {
                            if changed.is_err() || *cw_shutdown.borrow() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    }

    info!("Hybrid HID touch & keyboard active alongside AirPlay receiver");
    let mut scroll_position: Option<(u16, u16)> = None;

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            Some(command) = input_rx.recv() => {
                dispatch_hid_command(&mut hid, &mut keyboard, &mut scroll_position, command).await;
            }
            Some(button) = button_rx.recv() => {
                if let Some(indigo) = indigo_hid.as_mut() {
                    let (usage_page, usage_code) = button.usage();
                    if indigo.send_button(usage_page, usage_code, ButtonState::Down).await.is_ok() {
                        tokio::time::sleep(button.hold()).await;
                        let _ = indigo.send_button(usage_page, usage_code, ButtonState::Up).await;
                    }
                }
            }
            Some(content) = clipboard_rx.recv() => {
                if let Some(writer) = clipboard_writer.as_mut() {
                    match content {
                        ClipboardContent::Text(text) => {
                            let _ = writer.set_text(&text, GENERAL_PASTEBOARD).await;
                        }
                        ClipboardContent::Image { uti, bytes } => {
                            let _ = writer.set_image(&bytes, &uti, GENERAL_PASTEBOARD).await;
                        }
                    }
                }
            }
        }
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        hid.remove_main_keyboard(&mut keyboard),
    )
    .await;
}

async fn drain_airplay_inputs(
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<InputCommand>,
    button_rx: &mut tokio::sync::mpsc::UnboundedReceiver<HardwareButton>,
    clipboard_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClipboardContent>,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            Some(_) = input_rx.recv() => {}
            Some(_) = button_rx.recv() => {}
            Some(_) = clipboard_rx.recv() => {}
        }
    }
}

async fn run_airplay_mirror(settings: AppSettings) -> Result<()> {
    info!("Starting iphone-mirror-rs in optional AirPlay (H.264) Receiver mode...");

    let (mut audio_output, audio_ring, audio_mute) = create_output(14_400);
    audio_output.prepare()?;
    audio_output.start()?;
    info!(backend = audio_output.backend_name(), "AirPlay audio output started");

    let event_loop = winit::event_loop::EventLoop::<UiEvent>::with_user_event()
        .build()
        .context("Failed to create the macOS event loop")?;
    let proxy = event_loop.create_proxy();

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(4);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (button_tx, button_rx) = tokio::sync::mpsc::unbounded_channel::<HardwareButton>();
    let (clipboard_tx, clipboard_rx) =
        tokio::sync::mpsc::unbounded_channel::<ClipboardContent>();
    let audio_muted = Arc::new(AtomicBool::new(false));
    let decoded_frames = Arc::new(AtomicU64::new(0));

    let auto_rotate = settings.auto_rotate;
    let orient_proxy = proxy.clone();
    let orientation_cb: Arc<dyn Fn(DisplayRotation) + Send + Sync> = Arc::new(move |rotation| {
        if auto_rotate {
            let _ = orient_proxy.send_event(UiEvent::Orientation(rotation));
        }
    });

    let airplay = AirPlayReceiver::start(
        "iPhone Mirror".to_string(),
        frame_tx,
        decoded_frames,
        audio_ring.clone(),
        orientation_cb,
    )
    .await
    .context("Failed to start AirPlay receiver")?;
    info!(port = airplay.port, name = %airplay.receiver_name, "AirPlay H.264 receiver ready");

    let latest_frame: Arc<std::sync::Mutex<Option<DecodedFrame>>> =
        Arc::new(std::sync::Mutex::new(None));
    let frame_pending = Arc::new(AtomicBool::new(false));
    let latest_frame_tx = latest_frame.clone();
    let frame_pending_tx = frame_pending.clone();
    let frame_proxy = proxy.clone();
    tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if let Ok(mut slot) = latest_frame_tx.lock() {
                *slot = Some(frame);
            }
            if !frame_pending_tx.swap(true, Ordering::AcqRel)
                && frame_proxy.send_event(UiEvent::FrameReady).is_err()
            {
                break;
            }
        }
    });

    let signal_proxy = proxy.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                let _ = signal_proxy.send_event(UiEvent::ShutdownRequested);
            }
        }
    });

    let audio_muted_watch = audio_muted.clone();
    let mut audio_shutdown_rx = shutdown_rx.clone();
    let airplay_shutdown_proxy = proxy.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        let mut was_muted = false;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let muted = audio_muted_watch.load(Ordering::Relaxed);
                    if muted != was_muted {
                        audio_mute.set(muted);
                        if !muted {
                            audio_ring.clear();
                        }
                        was_muted = muted;
                    }
                }
                changed = audio_shutdown_rx.changed() => {
                    if changed.is_err() || *audio_shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        let _ = airplay_shutdown_proxy.send_event(UiEvent::ShutdownComplete);
    });

    let hid_proxy = proxy.clone();
    let hid_shutdown_rx = shutdown_rx.clone();
    std::thread::spawn(move || {
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            rt.block_on(run_airplay_hybrid_hid(
                input_rx,
                button_rx,
                clipboard_rx,
                hid_proxy,
                hid_shutdown_rx,
            ));
        }
    });

    let mut app = MirrorApp {
        window: None,
        renderer: None,
        latest_frame,
        frame_pending,
        shutdown_tx,
        frame_count: 0,
        frame_size: None,
        cursor_position: None,
        touch_active: false,
        scroll_active: false,
        scroll_deadline: None,
        scroll_started: false,
        scroll_accum: (0.0, 0.0),
        scroll_anchor: (u16::MAX / 2, u16::MAX / 2),
        suppress_click_until: None,
        last_touch_position: None,
        input_tx,
        button_tx,
        audio_muted,
        screenshot_requested: false,
        always_on_top: false,
        fullscreen: false,
        auto_rotate: settings.auto_rotate,
        rotation: DisplayRotation::Portrait,
        manual_rotation_override: false,
        frame_is_landscape: None,
        pressed_keys: BTreeSet::new(),
        modifiers: winit::keyboard::ModifiersState::empty(),
        clipboard: SystemClipboard::new(),
        clipboard_tx,
        last_device_clipboard: None,
        transfer_tx: None,
        drop_candidates: Vec::new(),
        drop_deadline: None,
        transfer_status: None,
        stream_error: None,
        shutting_down: false,
    };

    let loop_result = event_loop
        .run_app(&mut app)
        .context("macOS window event loop failed");
    airplay.stop();
    let _ = audio_output;
    let _ = shutdown_rx.changed();
    loop_result?;
    if let Some(error) = app.stream_error {
        anyhow::bail!("AirPlay mirror session failed: {error}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{offset_touch_position, rotated_window_size};

    #[test]
    fn scroll_position_moves_and_clamps_to_touchscreen_bounds() {
        assert_eq!(
            offset_touch_position((30_000, 40_000), (1_500, -2_000)),
            (31_500, 38_000)
        );
        assert_eq!(offset_touch_position((100, 100), (-1_000, -1_000)), (0, 0));
        assert_eq!(
            offset_touch_position((65_000, 65_000), (10_000, 10_000)),
            (u16::MAX, u16::MAX)
        );
    }

    #[test]
    fn auto_rotate_swaps_window_axes_and_preserves_minimums() {
        assert_eq!(rotated_window_size(420, 750, true), (750, 420));
        assert_eq!(rotated_window_size(750, 420, false), (420, 750));
        assert_eq!(rotated_window_size(100, 200, true), (500, 280));
    }
}

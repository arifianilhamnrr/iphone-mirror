// language: Rust, file: src/main.rs, target: macOS (Hackintosh / Mac)
use anyhow::{Context, Result};
use idevice::{
    core_device::{
        DataInclusionPolicy, PasteboardServiceClient, UniversalHidServiceClient, GENERAL_PASTEBOARD,
    },
    RsdService,
};
use iphone_mirror_rs::{
    audio::{AacEldDecoder, AudioPlayer},
    codec::{decoder::HardwareDecoder, format_desc::FormatDescription},
    input::{clipboard::SystemClipboard, coordinate_mapper::CoordinateMapper},
    media::nal_parser::ParameterSetCache,
    media::{h265_depacketizer::H265Depacketizer, rtp::RtpPacket},
    protocol::display_client::DisplayClient,
    transport::{device_scanner::DeviceScanner, rsd_tunnel::RsdTunnel},
    ui::metal_renderer::MetalRenderer,
};
use std::collections::BTreeSet;
use std::env;
use tracing::{debug, info, warn};

enum Command {
    Mirror,
    Devices,
    Diagnose { udid: Option<String> },
    Help,
}

enum UiEvent {
    Frame(iphone_mirror_rs::codec::decoder::DecodedFrame),
    ClipboardText(String),
    StreamError(String),
}

#[derive(Debug, Clone)]
enum InputCommand {
    Down(u16, u16),
    Move(u16, u16),
    Up(u16, u16),
    SetKeys(Vec<u16>),
}

struct MirrorApp {
    window: Option<winit::window::Window>,
    renderer: Option<MetalRenderer>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    frame_count: u64,
    frame_size: Option<(usize, usize)>,
    cursor_position: Option<winit::dpi::PhysicalPosition<f64>>,
    touch_active: bool,
    last_touch_position: Option<(u16, u16)>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputCommand>,
    pressed_keys: BTreeSet<u16>,
    clipboard: SystemClipboard,
    clipboard_tx: tokio::sync::mpsc::UnboundedSender<String>,
    last_device_clipboard: Option<String>,
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
            UiEvent::Frame(frame) => {
                self.frame_count += 1;
                self.frame_size = Some((frame.width(), frame.height()));
                if self.frame_count == 1 || self.frame_count % 300 == 0 {
                    info!(
                        frame_count = self.frame_count,
                        width = frame.width(),
                        height = frame.height(),
                        "Presenting Metal frame"
                    );
                }
                if let Some(renderer) = &self.renderer {
                    if let Err(error) = renderer.render_frame(&frame) {
                        warn!(%error, "Failed to present decoded frame");
                    }
                }
            }
            UiEvent::StreamError(error) => {
                tracing::error!(error, "Display stream stopped");
                event_loop.exit();
            }
            UiEvent::ClipboardText(text) => {
                if self.clipboard.current_text().as_deref() != Some(text.as_str()) {
                    if let Err(error) = self.clipboard.set_text(&text) {
                        warn!(%error, "Failed to copy iPhone clipboard text to macOS");
                    } else {
                        info!(bytes = text.len(), "Copied iPhone clipboard text to macOS");
                    }
                }
                self.last_device_clipboard = Some(text);
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        match event {
            winit::event::WindowEvent::CloseRequested => {
                let _ = self.shutdown_tx.send(true);
                event_loop.exit();
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
            winit::event::WindowEvent::CursorLeft { .. } if self.touch_active => {
                self.touch_active = false;
                if let Some((x, y)) = self.last_touch_position.take() {
                    let _ = self.input_tx.send(InputCommand::Up(x, y));
                }
            }
            winit::event::WindowEvent::KeyboardInput { event, .. } => {
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
            winit::event::WindowEvent::Focused(false) if !self.pressed_keys.is_empty() => {
                self.pressed_keys.clear();
                let _ = self.input_tx.send(InputCommand::SetKeys(Vec::new()));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if let Some(text) = self.clipboard.changed_text() {
            if self.last_device_clipboard.as_deref() != Some(text.as_str()) {
                let _ = self.clipboard_tx.send(text);
            }
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(250),
        ));
    }
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
    fn touch_coordinates(&self, position: winit::dpi::PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let window = self.window.as_ref()?;
        let (frame_width, frame_height) = self.frame_size?;
        let size = window.inner_size();
        let (x, y) = CoordinateMapper::map_aspect_fit(
            size.width as f64,
            size.height as f64,
            frame_width as f64,
            frame_height as f64,
            position.x,
            position.y,
        )?;
        Some((
            (x * u16::MAX as f32).round() as u16,
            (y * u16::MAX as f32).round() as u16,
        ))
    }
}

fn parse_command() -> Result<Command> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        None | Some("mirror") => Ok(Command::Mirror),
        Some("devices") => Ok(Command::Devices),
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
        Some("help" | "--help" | "-h") => Ok(Command::Help),
        Some(command) => anyhow::bail!("Unknown command: {command}. Run with --help for usage."),
    }
}

fn print_help() {
    println!(
        "iphone-mirror-rs\n\n\
         Usage:\n  \
           iphone-mirror [mirror]\n  \
           iphone-mirror devices\n  \
           iphone-mirror diagnose [--udid <UDID>]\n"
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
        Command::Mirror => run_mirror().await,
        Command::Devices => list_devices().await,
        Command::Diagnose { udid } => diagnose(udid.as_deref()).await,
        Command::Help => {
            print_help();
            Ok(())
        }
    }
}

async fn list_devices() -> Result<()> {
    let devices = DeviceScanner::list_devices()
        .await
        .context("Could not scan usbmuxd devices")?;

    if devices.is_empty() {
        println!("No connected iPhones found.");
        return Ok(());
    }

    for device in devices {
        println!(
            "{}\t{}\tdevice-id={}",
            device.udid, device.connection_type, device.device_id
        );
    }
    Ok(())
}

async fn diagnose(udid: Option<&str>) -> Result<()> {
    let device = DeviceScanner::select_device(udid).await?;
    println!("Device: {} ({})", device.udid, device.connection_type);
    println!("usbmuxd: connected");

    let tunnel = RsdTunnel::connect(Box::new(device.provider()))
        .await
        .context("CoreDevice/RSD diagnostics failed")?;

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

async fn run_mirror() -> Result<()> {
    info!("Starting iphone-mirror-rs (Native 60fps macOS/Hackintosh Mirror)...");

    let device = DeviceScanner::select_device(None).await?;
    info!(udid = %device.udid, conn = %device.connection_type, "iPhone detected");

    let mut tunnel = RsdTunnel::connect(Box::new(device.provider())).await?;
    if !tunnel.has_display_service() {
        anyhow::bail!(
            "Display service is unavailable. Enable Developer Mode and ensure the developer image is mounted."
        );
    }

    let mut client = DisplayClient::start(&mut tunnel, 1)
        .await
        .context("Failed to start the display service stream")?;
    info!("Stream session negotiated; awaiting H.265 RTP packets");
    let mut hid =
        UniversalHidServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake)
            .await
            .context("Failed to connect to the universal HID service")?;
    let mut keyboard = hid.create_main_keyboard().await.map_err(|error| {
        anyhow::anyhow!("Failed to create the iPhone keyboard surface: {error}")
    })?;
    let mut clipboard_writer =
        PasteboardServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake)
            .await
            .context("Failed to connect to the pasteboard service")?;
    let mut clipboard_watcher =
        PasteboardServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake)
            .await
            .context("Failed to open the pasteboard notification channel")?;
    clipboard_watcher
        .set_change_notifications(
            true,
            GENERAL_PASTEBOARD,
            Some(DataInclusionPolicy::AllResolved),
        )
        .await
        .context("Failed to subscribe to iPhone clipboard changes")?;

    let event_loop = winit::event_loop::EventLoop::<UiEvent>::with_user_event()
        .build()
        .context("Failed to create the macOS event loop")?;
    let proxy = event_loop.create_proxy();
    let stream_proxy = proxy.clone();
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(3);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (clipboard_tx, mut clipboard_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let clipboard_proxy = stream_proxy.clone();
    let mut clipboard_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                snapshot = clipboard_watcher.recv_push() => match snapshot {
                    Ok(snapshot) => {
                        if let Some(text) = snapshot.text() {
                            if clipboard_proxy.send_event(UiEvent::ClipboardText(text)).is_err() {
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

    tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if proxy.send_event(UiEvent::Frame(frame)).is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let result: Result<()> = async {
            let mut depacketizer = H265Depacketizer::new();
            let mut parameter_sets = ParameterSetCache::new();
            let mut decoder: Option<HardwareDecoder> = None;
            let mut nal_count = 0_u64;
            let mut audio_packet_count = 0_u64;
            let mut audio_decoder = AacEldDecoder::new()?;
            let mut audio_player = AudioPlayer::new()?;

            loop {
                tokio::select! {
                    packet = client.recv_video_packet() => {
                        let packet = packet?;
                        if RtpPacket::is_rtcp(&packet) {
                            continue;
                        }
                        match RtpPacket::parse(&packet).and_then(|packet| depacketizer.process_packet(&packet)) {
                            Ok(nals) => for nal in nals {
                                nal_count += 1;
                                parameter_sets.inspect_and_cache(&nal);
                                if decoder.is_none() && parameter_sets.is_complete() {
                                    let format = FormatDescription::from_hevc_parameter_sets(
                                        parameter_sets.vps.as_deref().expect("cache complete"),
                                        parameter_sets.sps.as_deref().expect("cache complete"),
                                        parameter_sets.pps.as_deref().expect("cache complete"),
                                    )?;
                                    decoder = Some(HardwareDecoder::new(format, frame_tx.clone())?);
                                }
                                if let Some(decoder) = decoder.as_mut() {
                                    if let Err(error) = decoder.decode_nal(&nal) {
                                        debug!(%error, "VideoToolbox rejected a NAL unit");
                                    }
                                }
                                if nal_count % 300 == 0 {
                                    info!(nal_count, "Receiving active HEVC stream");
                                }
                            },
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
                        match audio_decoder.decode(&packet.payload) {
                            Ok(pcm) => audio_player.play(&pcm)?,
                            Err(error) => debug!(%error, "Dropped invalid AAC-ELD audio packet"),
                        }
                        if audio_packet_count % 500 == 0 {
                            info!(audio_packet_count, "Playing iPhone system audio");
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    Some(command) = input_rx.recv() => {
                        debug!(?command, "Sending HID input event");
                        let command_kind = match &command {
                            InputCommand::Down(..) => "touch-down",
                            InputCommand::Move(..) => "touch-move",
                            InputCommand::Up(..) => "touch-up",
                            InputCommand::SetKeys(..) => "keyboard",
                        };
                        let result = match command {
                            InputCommand::Down(x, y) | InputCommand::Move(x, y) => {
                                hid.send_touchscreen(
                                    idevice::core_device::TOUCHSCREEN_STATE_CONTACT,
                                    x,
                                    y,
                                    None,
                                ).await
                            }
                            InputCommand::Up(x, y) => {
                                hid.send_touchscreen(
                                    idevice::core_device::TOUCHSCREEN_STATE_RELEASE,
                                    x,
                                    y,
                                    None,
                                ).await
                            }
                            InputCommand::SetKeys(usages) => {
                                let usages = usages.into_iter()
                                    .map(idevice::core_device::KeyboardUsage::new)
                                    .collect::<std::result::Result<Vec<_>, _>>()
                                    .map_err(|error| idevice::IdeviceError::UnexpectedResponse(error.to_string()))?;
                                hid.set_main_keyboard_usages(&mut keyboard, usages)
                                    .await
                                    .map_err(|error| idevice::IdeviceError::UnexpectedResponse(error.to_string()))
                            }
                        };
                        if let Err(error) = result {
                            warn!(%error, command_kind, "Failed to send HID input event");
                        }
                    }
                    Some(text) = clipboard_rx.recv() => {
                        if let Err(error) = clipboard_writer.set_text(&text, GENERAL_PASTEBOARD).await {
                            warn!(%error, "Failed to copy macOS clipboard text to iPhone");
                        } else {
                            info!(bytes = text.len(), "Copied macOS clipboard text to iPhone");
                        }
                    }
                }
            }
            if let Err(error) = hid.remove_main_keyboard(&mut keyboard).await {
                warn!(%error, "Failed to remove the virtual keyboard service");
            }
            if let Err(error) = client.stop().await {
                warn!(%error, "Display stream shutdown was not acknowledged");
            }
            Ok(())
        }.await;
        if let Err(error) = result {
            let _ = stream_proxy.send_event(UiEvent::StreamError(error.to_string()));
        }
    });

    let mut app = MirrorApp {
        window: None,
        renderer: None,
        shutdown_tx,
        frame_count: 0,
        frame_size: None,
        cursor_position: None,
        touch_active: false,
        last_touch_position: None,
        input_tx,
        pressed_keys: BTreeSet::new(),
        clipboard: SystemClipboard::new(),
        clipboard_tx,
        last_device_clipboard: None,
    };
    event_loop
        .run_app(&mut app)
        .context("macOS window event loop failed")?;
    Ok(())
}

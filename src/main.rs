// language: Rust, file: src/main.rs, target: macOS (Hackintosh / Mac)
use anyhow::{Context, Result};
use idevice::{core_device::UniversalHidServiceClient, RsdService};
use iphone_mirror_rs::{
    codec::{decoder::HardwareDecoder, format_desc::FormatDescription},
    input::coordinate_mapper::CoordinateMapper,
    media::nal_parser::ParameterSetCache,
    media::{h265_depacketizer::H265Depacketizer, rtp::RtpPacket},
    protocol::display_client::DisplayClient,
    transport::{device_scanner::DeviceScanner, rsd_tunnel::RsdTunnel},
    ui::metal_renderer::MetalRenderer,
};
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
    StreamError(String),
}

#[derive(Debug, Clone, Copy)]
enum TouchCommand {
    Down(u16, u16),
    Move(u16, u16),
    Up(u16, u16),
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
    touch_tx: tokio::sync::mpsc::UnboundedSender<TouchCommand>,
}

impl winit::application::ApplicationHandler<UiEvent> for MirrorApp {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
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
                        let _ = self.touch_tx.send(TouchCommand::Move(x, y));
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
                            let _ = self.touch_tx.send(TouchCommand::Down(x, y));
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
                            let _ = self.touch_tx.send(TouchCommand::Up(x, y));
                        }
                    }
                }
            },
            winit::event::WindowEvent::CursorLeft { .. } if self.touch_active => {
                self.touch_active = false;
                if let Some((x, y)) = self.last_touch_position.take() {
                    let _ = self.touch_tx.send(TouchCommand::Up(x, y));
                }
            }
            _ => {}
        }
    }
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

    let event_loop = winit::event_loop::EventLoop::<UiEvent>::with_user_event()
        .build()
        .context("Failed to create the macOS event loop")?;
    let proxy = event_loop.create_proxy();
    let stream_proxy = proxy.clone();
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(3);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (touch_tx, mut touch_rx) = tokio::sync::mpsc::unbounded_channel();

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
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    Some(command) = touch_rx.recv() => {
                        debug!(?command, "Sending HID touch event");
                        let result = match command {
                            TouchCommand::Down(x, y) | TouchCommand::Move(x, y) => {
                                hid.send_touchscreen(
                                    idevice::core_device::TOUCHSCREEN_STATE_CONTACT,
                                    x,
                                    y,
                                    None,
                                ).await
                            }
                            TouchCommand::Up(x, y) => {
                                hid.send_touchscreen(
                                    idevice::core_device::TOUCHSCREEN_STATE_RELEASE,
                                    x,
                                    y,
                                    None,
                                ).await
                            }
                        };
                        if let Err(error) = result {
                            warn!(%error, ?command, "Failed to send touch event");
                        }
                    }
                }
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
        touch_tx,
    };
    event_loop
        .run_app(&mut app)
        .context("macOS window event loop failed")?;
    Ok(())
}

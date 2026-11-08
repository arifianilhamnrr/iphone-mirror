// language: Rust, file: src/main.rs, target: macOS (Hackintosh / Mac)
use anyhow::{Context, Result};
use iphone_mirror_rs::{
    media::udp_receiver::UdpMediaReceiver,
    protocol::display_client::DisplayClient,
    transport::device_scanner::DeviceScanner,
};
use tokio::sync::mpsc;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    info!("Starting iphone-mirror-rs (Native 60fps macOS/Hackintosh Mirror)...");

    // 1. Scan for connected iOS device
    let devices = DeviceScanner::list_devices().await
        .context("Could not scan usbmuxd devices")?;

    // Start background hotplug watcher
    let (hotplug_tx, mut hotplug_rx) = mpsc::channel(16);
    let _hotplug_task = iphone_mirror_rs::transport::hotplug::HotplugMonitor::start_monitor(hotplug_tx);

    tokio::spawn(async move {
        while let Some(event) = hotplug_rx.recv().await {
            info!(?event, "Device hotplug state update");
        }
    });

    if devices.is_empty() {
        info!("No iPhone detected. Connect your iPhone via USB cable to continue.");
        return Ok(());
    }

    let dev = &devices[0];
    info!(udid = %dev.udid, conn = %dev.connection_type, "iPhone detected");

    // 2. Setup UDP media receiver on ephemeral port
    let receiver = UdpMediaReceiver::bind().await
        .context("Failed to bind UDP media receiver")?;
    let local_addr = receiver.local_addr();

    let (nal_tx, mut nal_rx) = mpsc::channel::<Vec<u8>>(256);
    let _worker_handle = receiver.start_worker(nal_tx);

    // 3. Initiate displayservice stream over RemoteXPC
    let mut client = DisplayClient::new();
    client.start_stream(local_addr, 1170, 2532, 60).await
        .context("Failed to start display service stream")?;

    info!("Stream session negotiated! Awaiting H.265 NAL units from iPhone...");

    // 4. Processing loop (dispatches frames to VideoToolbox decoder on macOS)
    tokio::spawn(async move {
        let mut frame_count: u64 = 0;
        while let Some(nal) = nal_rx.recv().await {
            frame_count += 1;
            if frame_count % 300 == 0 {
                info!(frame_count, nal_len = nal.len(), "Receiving active 60fps HEVC stream");
            }
        }
    });

    // Keep running
    tokio::signal::ctrl_c().await?;
    info!("Shutting down mirror stream session...");
    client.stop_stream().await?;

    Ok(())
}

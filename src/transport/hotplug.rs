// language: Rust, file: src/transport/hotplug.rs, target: macOS / Linux
use crate::transport::device_scanner::DeviceScanner;
use anyhow::Result;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Attached(String), // UDID
    Detached(String), // UDID
}

pub struct HotplugMonitor;

impl HotplugMonitor {
    /// Polls usbmuxd device list and detects plug/unplug events
    pub fn start_monitor(tx: mpsc::Sender<DeviceEvent>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut known_devices: Vec<String> = Vec::new();
            info!("Hotplug device monitor started");

            loop {
                tokio::time::sleep(Duration::from_millis(1000)).await;

                if let Ok(current_list) = DeviceScanner::list_devices().await {
                    let current_udids: Vec<String> = current_list.into_iter().map(|d| d.udid).collect();

                    // Check for newly attached devices
                    for udid in &current_udids {
                        if !known_devices.contains(udid) {
                            info!(udid = %udid, "Device attached");
                            let _ = tx.send(DeviceEvent::Attached(udid.clone())).await;
                        }
                    }

                    // Check for detached devices
                    for udid in &known_devices {
                        if !current_udids.contains(udid) {
                            info!(udid = %udid, "Device detached");
                            let _ = tx.send(DeviceEvent::Detached(udid.clone())).await;
                        }
                    }

                    known_devices = current_udids;
                } else {
                    debug!("usbmuxd scan tick skipped");
                }
            }
        })
    }
}

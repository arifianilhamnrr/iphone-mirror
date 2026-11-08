// language: Rust, file: src/transport/device_scanner.rs, target: macOS / Linux
use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ConnectedDevice {
    pub udid: String,
    pub connection_type: String,
    pub device_id: u32,
}

pub struct DeviceScanner;

impl DeviceScanner {
    /// Check whether native usbmuxd UNIX domain socket is available
    pub fn socket_path() -> &'static str {
        if cfg!(target_os = "macos") {
            "/var/run/usbmuxd"
        } else {
            "/var/run/usbmuxd"
        }
    }

    pub fn is_usbmuxd_running() -> bool {
        Path::new(Self::socket_path()).exists()
    }

    /// Discover devices via usbmuxd
    pub async fn list_devices() -> Result<Vec<ConnectedDevice>> {
        if !Self::is_usbmuxd_running() {
            anyhow::bail!(
                "usbmuxd socket not found at {}. Ensure Apple MobileDevice/usbmuxd is running.",
                Self::socket_path()
            );
        }

        // Connect to usbmuxd
        let mut mux = idevice::usbmuxd::UsbmuxdConnection::connect()
            .await
            .context("Failed to connect to usbmuxd daemon socket")?;

        let devices = mux
            .list_devices()
            .await
            .context("Failed to query device list from usbmuxd")?;

        let list = devices
            .into_iter()
            .map(|dev| ConnectedDevice {
                udid: dev.serial.clone(),
                connection_type: format!("{:?}", dev.connection_type),
                device_id: dev.device_id,
            })
            .collect();

        Ok(list)
    }
}

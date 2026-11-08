// language: Rust, file: src/transport/device_scanner.rs, target: macOS / Linux
use anyhow::{Context, Result};
use idevice::provider::UsbmuxdProvider;
use idevice::usbmuxd::{Connection, UsbmuxdAddr};
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionType {
    Usb,
    Network,
    Unknown(String),
}

impl fmt::Display for ConnectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usb => f.write_str("USB"),
            Self::Network => f.write_str("Network"),
            Self::Unknown(value) => write!(f, "Unknown ({value})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectedDevice {
    pub udid: String,
    pub connection_type: ConnectionType,
    pub device_id: u32,
}

impl ConnectedDevice {
    pub fn provider(&self) -> UsbmuxdProvider {
        UsbmuxdProvider {
            addr: UsbmuxdAddr::default(),
            tag: 0,
            udid: self.udid.clone(),
            device_id: self.device_id,
            label: "iphone-mirror-rs".to_string(),
        }
    }
}

pub struct DeviceScanner;

impl DeviceScanner {
    /// Check whether native usbmuxd UNIX domain socket is available
    pub fn socket_path() -> &'static str {
        "/var/run/usbmuxd"
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
        let mut mux = idevice::usbmuxd::UsbmuxdConnection::default()
            .await
            .context("Failed to connect to usbmuxd daemon socket")?;

        let devices = mux
            .get_devices()
            .await
            .context("Failed to query device list from usbmuxd")?;

        let list = devices
            .into_iter()
            .map(|dev| ConnectedDevice {
                udid: dev.udid,
                connection_type: match dev.connection_type {
                    Connection::Usb => ConnectionType::Usb,
                    Connection::Network(_) => ConnectionType::Network,
                    Connection::Unknown(value) => ConnectionType::Unknown(value),
                },
                device_id: dev.device_id,
            })
            .collect();

        Ok(list)
    }

    pub async fn select_device(udid: Option<&str>) -> Result<ConnectedDevice> {
        let devices = Self::list_devices().await?;

        if let Some(udid) = udid {
            return devices
                .into_iter()
                .find(|device| device.udid == udid)
                .with_context(|| format!("No connected iPhone matches UDID {udid}"));
        }

        match devices.len() {
            0 => anyhow::bail!("No iPhone detected. Connect and trust an iPhone first."),
            1 => Ok(devices.into_iter().next().expect("length checked")),
            count => anyhow::bail!(
                "Found {count} iPhones. Select one with `iphone-mirror diagnose --udid <UDID>`."
            ),
        }
    }
}

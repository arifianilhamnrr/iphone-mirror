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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportPreference {
    #[default]
    Auto,
    Usb,
    Wifi,
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
        Self::select_device_with_transport(udid, TransportPreference::Auto).await
    }

    pub async fn select_device_with_transport(
        udid: Option<&str>,
        transport: TransportPreference,
    ) -> Result<ConnectedDevice> {
        let mut devices = Self::list_devices().await?;

        devices.retain(|device| match transport {
            TransportPreference::Auto => true,
            TransportPreference::Usb => device.connection_type == ConnectionType::Usb,
            TransportPreference::Wifi => device.connection_type == ConnectionType::Network,
        });

        if let Some(udid) = udid {
            devices.retain(|device| device.udid == udid);
        }

        // Finder Wi-Fi sync can expose one UDID over USB and Network at the
        // same time. Treat that as one phone, preferring USB in Auto mode.
        devices.sort_by_key(|device| match device.connection_type {
            ConnectionType::Usb => 0,
            ConnectionType::Network => 1,
            ConnectionType::Unknown(_) => 2,
        });
        let unique_udids = devices
            .iter()
            .map(|device| device.udid.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        if unique_udids.len() == 1 {
            return Ok(devices.remove(0));
        }

        match unique_udids.len() {
            0 if udid.is_some() => anyhow::bail!(
                "No connected iPhone matches UDID {} over {}",
                udid.expect("checked"),
                transport.label()
            ),
            0 => anyhow::bail!(
                "No iPhone detected over {}. Connect and trust an iPhone first.",
                transport.label()
            ),
            count => anyhow::bail!(
                "Found {count} iPhones. Select one with `iphone-mirror diagnose --udid <UDID>`."
            ),
        }
    }
}

impl TransportPreference {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "USB or Wi-Fi",
            Self::Usb => "USB",
            Self::Wifi => "Wi-Fi",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TransportPreference;

    #[test]
    fn transport_labels_are_human_readable() {
        assert_eq!(TransportPreference::Auto.label(), "USB or Wi-Fi");
        assert_eq!(TransportPreference::Wifi.label(), "Wi-Fi");
    }
}

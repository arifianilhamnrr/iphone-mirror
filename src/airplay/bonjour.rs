// language: Rust, file: src/airplay/bonjour.rs
//! Advertise `_airplay._tcp` and `_raop._tcp` over Bonjour using macOS `dns-sd`.

use anyhow::{Context, Result};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::info;

pub struct BonjourRegistration {
    _airplay: Child,
    _raop: Child,
}

impl BonjourRegistration {
    /// Register both `_airplay._tcp` and `_raop._tcp` on `port`.
    /// Dropping the returned guard terminates both `dns-sd -R` processes.
    pub fn register(name: &str, port: u16, device_id: &str, pk_hex: &str) -> Result<Self> {
        let port_str = port.to_string();
        let device_id_txt = format!("deviceid={device_id}");
        let pk_txt = format!("pk={pk_hex}");

        let airplay = Command::new("dns-sd")
            .args([
                "-R",
                name,
                "_airplay._tcp",
                "local",
                &port_str,
                &device_id_txt,
                "features=0x5A7FFFF7,0x1E",
                "flags=0x4",
                "model=AppleTV3,2",
                "pi=b08f5a79-db29-4384-b456-a4784d9e6055",
                &pk_txt,
                "srcvers=220.68",
                "vv=2",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn dns-sd for _airplay._tcp")?;

        let mac_prefix = device_id.replace(':', "");
        let raop_name = format!("{mac_prefix}@{name}");
        let raop = Command::new("dns-sd")
            .args([
                "-R",
                &raop_name,
                "_raop._tcp",
                "local",
                &port_str,
                "am=AppleTV3,2",
                "ch=2",
                "cn=0,1,2,3",
                "da=true",
                "et=0,3,5",
                "ft=0x5A7FFFF7,0x1E",
                "md=0,1,2",
                &pk_txt,
                "pw=false",
                "rhd=5.6.0.0",
                "sf=0x4",
                "sr=44100",
                "ss=16",
                "sv=false",
                "tp=UDP",
                "txtvers=1",
                "vn=65537",
                "vs=220.68",
                "vv=2",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn dns-sd for _raop._tcp")?;

        info!(name, port, device_id, "Registered AirPlay Bonjour services");
        Ok(Self {
            _airplay: airplay,
            _raop: raop,
        })
    }
}

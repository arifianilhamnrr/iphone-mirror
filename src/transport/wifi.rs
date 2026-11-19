// language: Rust, file: src/transport/wifi.rs
//! Wi-Fi transport discovery and tunnel setup via Bonjour + RemotePairing.
//!
//! iOS advertises `_remotepairing._tcp` when "Show this iPhone when on Wi-Fi"
//! is enabled in Finder. A device found there can be paired once and then
//! reached over a TLS-PSK tunnel without usbmuxd.

use anyhow::{Context, Result};
use idevice::{
    IdeviceService, RemoteXpcClient,
    core_device_proxy::CoreDeviceProxy,
    provider::IdeviceProvider,
    remote_pairing::{
        RemotePairingClient, RpPairingFile, RpPairingSocket, connect_tls_psk_tunnel_native,
    },
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
};
use serde::Serialize;
use std::{
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpStream,
    process::Command,
};
use tracing::{debug, info};

/// Stable host name presented during RemotePairing. It must not change
/// between runs, otherwise the saved pairing file no longer matches.
const SENDING_HOST: &str = "iphone-mirror-rs";
const BROWSE_SECONDS: u64 = 1;
const RESOLVE_SECONDS: u64 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct WifiDevice {
    /// Bonjour instance name, used as a stable UI identifier.
    pub name: String,
    /// The `identifier` TXT value when advertised.
    pub identifier: String,
    pub host: String,
    pub port: u16,
}

pub struct WifiDiscovery;

impl WifiDiscovery {
    /// Discover iPhones advertising the RemotePairing service via `dns-sd`.
    pub async fn discover() -> Result<Vec<WifiDevice>> {
        let browse = run_dns_sd(
            &["-B", "_remotepairing._tcp"],
            Duration::from_secs(BROWSE_SECONDS),
        )
        .await?;

        let mut names = Vec::new();
        for line in browse.lines() {
            if !line.contains("Add") {
                continue;
            }
            let Some(marker) = line.find(" _remotepairing._tcp.") else {
                continue;
            };
            let name = line[marker + " _remotepairing._tcp.".len()..].trim();
            if !name.is_empty() && !names.iter().any(|existing: &String| existing == name) {
                names.push(name.to_string());
            }
        }
        if names.is_empty() {
            return Ok(Vec::new());
        }

        let mut devices = Vec::new();
        for name in names {
            let args = ["-L", name.as_str(), "_remotepairing._tcp", "local."];
            match run_dns_sd(&args, Duration::from_secs(RESOLVE_SECONDS)).await {
                Ok(output) => {
                    if let Some(device) = parse_resolved(&name, &output) {
                        devices.push(device);
                    }
                }
                Err(error) => debug!(%error, %name, "Could not resolve RemotePairing service"),
            }
        }
        Ok(devices)
    }
}

/// Load the pairing file for this host, generating a fresh one on first use.
pub fn pairing_file_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/iPhone Mirror")
        .join("rppairing.plist"))
}

async fn load_or_create_pairing_file() -> Result<RpPairingFile> {
    let path = pairing_file_path()?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match RpPairingFile::read_from_file(&path).await {
        Ok(file) => Ok(file),
        Err(_) => {
            let file = RpPairingFile::generate(SENDING_HOST);
            file.write_to_file(&path).await?;
            info!(path = %path.display(), "Generated a new RemotePairing identity");
            Ok(file)
        }
    }
}

/// Pair with the connected device over USB and persist the pairing file so
/// subsequent sessions can use the Wi-Fi tunnel.
pub async fn pair_via_usb(provider: &dyn IdeviceProvider) -> Result<()> {
    let proxy = CoreDeviceProxy::connect(provider)
        .await
        .context("Could not connect to CoreDeviceProxy over USB")?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let mut adapter = proxy
        .create_software_tunnel()
        .context("Could not create the software tunnel")?
        .to_async_handle();
    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;
    let service = handshake
        .services
        .get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
        .context("Untrusted tunnel service is unavailable")?;
    let stream = adapter.connect(service.port).await?;
    let mut client = RemoteXpcClient::new(stream).await?;
    client.do_handshake().await?;
    let _ = client.recv_root().await?;

    let mut file = RpPairingFile::generate(SENDING_HOST);
    let mut pairing = RemotePairingClient::new(client, SENDING_HOST);
    pairing
        .connect(&mut file, || async { "000000".to_string() })
        .await
        .context("RemotePairing failed; accept the pairing prompt on the iPhone")?;

    let path = pairing_file_path()?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    file.write_to_file(&path).await?;
    info!(path = %path.display(), "Saved RemotePairing file");
    Ok(())
}

/// Connect to a Wi-Fi device and return a ready RSD adapter + handshake.
pub async fn connect(device: &WifiDevice) -> Result<(AdapterHandle, RsdHandshake)> {
    let path = pairing_file_path()?;
    let mut file = load_or_create_pairing_file().await?;

    let pairing_addr = format!("{}:{}", device.host, device.port);
    info!(%pairing_addr, "Connecting to the iPhone over Wi-Fi");
    let stream = TcpStream::connect(&pairing_addr)
        .await
        .with_context(|| format!("Could not reach {pairing_addr}"))?;
    let socket = RpPairingSocket::new(stream);
    let mut pairing = RemotePairingClient::new(socket, SENDING_HOST);
    pairing
        .connect(&mut file, || async { "000000".to_string() })
        .await
        .context("RemotePairing with the Wi-Fi device failed")?;
    file.write_to_file(&path)
        .await
        .context("Could not persist the RemotePairing file")?;

    let tunnel_port = pairing.create_tcp_listener().await?;
    let tunnel_addr = format!("{}:{}", device.host, tunnel_port);
    let tunnel_stream = TcpStream::connect(&tunnel_addr)
        .await
        .with_context(|| format!("Could not reach the tunnel at {tunnel_addr}"))?;
    let tunnel = connect_tls_psk_tunnel_native(tunnel_stream, pairing.encryption_key())
        .await
        .context("TLS-PSK tunnel handshake failed")?;

    let client_ip = tunnel.info.client_address.parse()?;
    let server_ip = tunnel.info.server_address.parse()?;
    let mtu = tunnel.info.mtu as usize;
    let rsd_port = tunnel.info.server_rsd_port;
    let info = tunnel.info.clone();
    let mut adapter = idevice::tcp::adapter::Adapter::new(Box::new(tunnel.into_inner()), client_ip, server_ip);
    adapter.set_mss(mtu.saturating_sub(60).min(8192));
    let mut adapter = adapter.to_async_handle();
    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;
    info!(?info, "Wi-Fi RSD tunnel established");
    Ok((adapter, handshake))
}

fn parse_resolved(name: &str, output: &str) -> Option<WifiDevice> {
    for line in output.lines() {
        if let Some(at) = line.find(" can be reached at ") {
            let endpoint = line[at + " can be reached at ".len()..]
                .split_whitespace()
                .next()?;
            let (host, port) = endpoint.rsplit_once(':')?;
            let host = host.trim_end_matches('.').to_string();
            let port = port.parse().ok()?;
            let identifier = output
                .lines()
                .find_map(|line| line.trim().strip_prefix("identifier="))
                .map(|value| value.split_whitespace().next().unwrap_or(name))
                .unwrap_or(name)
                .to_string();
            return Some(WifiDevice {
                name: name.to_string(),
                identifier,
                host,
                port,
            });
        }
    }
    None
}

async fn run_dns_sd(args: &[&str], duration: Duration) -> Result<String> {
    let mut child = Command::new("dns-sd")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Could not run dns-sd; Bonjour discovery is unavailable")?;
    let stdout = child.stdout.take().context("dns-sd had no stdout")?;
    let mut reader = BufReader::new(stdout);
    let mut output = String::new();
    let mut line = String::new();
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        let read = tokio::select! {
            result = reader.read_line(&mut line) => result,
            _ = tokio::time::sleep_until(deadline) => break,
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                output.push_str(&line);
                line.clear();
            }
        }
    }
    let _ = child.kill().await;
    Ok(output)
}

/// Wi-Fi endpoint passed by the launcher, which resolves Bonjour itself.
pub fn endpoint_from_env() -> Option<WifiDevice> {
    let host = std::env::var("IPHONE_MIRROR_WIFI_HOST").ok()?;
    let port = std::env::var("IPHONE_MIRROR_WIFI_PORT")
        .ok()?
        .parse()
        .ok()?;
    Some(WifiDevice {
        name: std::env::var("IPHONE_MIRROR_WIFI_NAME")
            .unwrap_or_else(|_| host.clone()),
        identifier: std::env::var("IPHONE_MIRROR_WIFI_IDENTIFIER")
            .unwrap_or_else(|_| host.clone()),
        host,
        port,
    })
}

// language: Rust, file: src/transport/wifi.rs
//! Wi-Fi transport discovery and tunnel setup via Bonjour/Avahi + RemotePairing.
//!
//! iOS advertises `_remotepairing._tcp` when "Show this iPhone when on Wi-Fi"
//! is enabled. A device found there can be paired once and then
//! reached over a TLS-PSK tunnel without usbmuxd.

use anyhow::{Context, Result};
use idevice::{
    core_device_proxy::CoreDeviceProxy,
    provider::IdeviceProvider,
    remote_pairing::{
        connect_tls_psk_tunnel_native, RemotePairingClient, RpPairingFile, RpPairingSocket,
    },
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    IdeviceService, RemoteXpcClient,
};
use serde::Serialize;
use std::{path::PathBuf, process::Stdio, time::Duration};
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
    /// Discover iPhones advertising `_remotepairing._tcp` via `avahi-browse` (Linux) or `dns-sd` (macOS).
    pub async fn discover() -> Result<Vec<WifiDevice>> {
        #[cfg(not(target_os = "macos"))]
        {
            if let Ok(output) = run_tool(
                "avahi-browse",
                &["-prt", "_remotepairing._tcp"],
                Duration::from_millis(1500),
            )
            .await
            {
                let devices = parse_avahi_browse_output(&output);
                if !devices.is_empty() {
                    return Ok(devices);
                }
            }
        }

        let browse = run_tool(
            "dns-sd",
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

        let mut devices: Vec<WifiDevice> = Vec::new();
        for name in names {
            let args = ["-L", name.as_str(), "_remotepairing._tcp", "local."];
            match run_tool("dns-sd", &args, Duration::from_secs(RESOLVE_SECONDS)).await {
                Ok(output) => {
                    if let Some(device) = parse_resolved(&name, &output) {
                        upsert_wifi_device(&mut devices, device);
                    }
                }
                Err(error) => debug!(%error, %name, "Could not resolve RemotePairing service"),
            }
        }
        Ok(devices)
    }
}

/// Replace any earlier mDNS entry sharing the same host (ignoring case / trailing dot)
/// so rotated iOS RemotePairing UUIDs do not produce duplicate devices in the UI.
pub fn upsert_wifi_device(devices: &mut Vec<WifiDevice>, device: WifiDevice) {
    let key = device.host.trim_end_matches('.').to_ascii_lowercase();
    if let Some(existing) = devices
        .iter_mut()
        .find(|d| d.host.trim_end_matches('.').to_ascii_lowercase() == key)
    {
        *existing = device;
    } else {
        devices.push(device);
    }
}

/// Load the pairing file for this host, generating a fresh one on first use.
pub fn pairing_file_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library/Application Support/iPhone Mirror")
            .join("rppairing.plist"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg)
        } else {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".config")
        };
        Ok(base.join("iphone-mirror").join("rppairing.plist"))
    }
}

fn read_local_hostname() -> String {
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| SENDING_HOST.to_string())
}

fn try_import_pymobiledevice3_pairing() -> Option<RpPairingFile> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".local/share/pymobiledevice3");
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("remote_") && n.ends_with(".plist"))
        })
        .collect();
    candidates.sort();

    let hostname = read_local_hostname();
    let fallback_id = uuid::Uuid::new_v3(&uuid::Uuid::NAMESPACE_DNS, hostname.as_bytes())
        .to_string()
        .to_uppercase();

    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(mut dict) = plist::from_bytes::<plist::Dictionary>(&bytes) else {
            continue;
        };
        let Some(pub_bytes) = dict.remove("public_key").and_then(|v| v.into_data()) else {
            continue;
        };
        let Some(priv_bytes) = dict.remove("private_key").and_then(|v| v.into_data()) else {
            continue;
        };
        let identifier = dict
            .remove("host_identifier")
            .or_else(|| dict.remove("identifier"))
            .and_then(|v| v.into_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| fallback_id.clone());
        let alt_irk = dict
            .remove("peer_alt_irk")
            .or_else(|| dict.remove("alt_irk"))
            .and_then(|v| v.into_data());

        let mut out = plist::Dictionary::new();
        out.insert("public_key".into(), plist::Value::Data(pub_bytes));
        out.insert("private_key".into(), plist::Value::Data(priv_bytes));
        out.insert("identifier".into(), plist::Value::String(identifier));
        if let Some(irk) = alt_irk {
            out.insert("alt_irk".into(), plist::Value::Data(irk));
        }
        let mut xml = Vec::new();
        if plist::Value::Dictionary(out)
            .to_writer_xml(&mut xml)
            .is_ok()
        {
            if let Ok(rp) = RpPairingFile::from_bytes(&xml) {
                info!(source = %path.display(), "Imported existing RemotePairing record");
                return Some(rp);
            }
        }
    }
    None
}

async fn load_or_create_pairing_file() -> Result<RpPairingFile> {
    let path = pairing_file_path()?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match RpPairingFile::read_from_file(&path).await {
        Ok(file) => Ok(file),
        Err(_) => {
            let file = try_import_pymobiledevice3_pairing()
                .unwrap_or_else(|| RpPairingFile::generate(SENDING_HOST));
            file.write_to_file(&path).await?;
            info!(path = %path.display(), "Saved RemotePairing identity");
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

/// Connect to a TCP host:port preferring IPv4 addresses before IPv6 link-local addresses,
/// and attach actionable context when macOS Local Network privacy blocks the socket (`EHOSTUNREACH`).
async fn connect_tcp_prefer_ipv4(addr: &str) -> Result<TcpStream> {
    let mut addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("Could not resolve {addr}"))?
        .collect();
    // Try IPv4 first so link-local IPv6 quirks on macOS don't fail with early EOF
    // and mask an IPv4 Local Network permission block (os error 65).
    addrs.sort_by_key(|a| if a.is_ipv4() { 0_u8 } else { 1_u8 });

    let mut last_err: Option<std::io::Error> = None;
    for target in addrs {
        match TcpStream::connect(target).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                debug!(%target, %err, "TCP connect attempt failed");
                last_err = Some(err);
            }
        }
    }

    let err = last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no resolved addresses",
        )
    });
    #[cfg(target_os = "macos")]
    if err.raw_os_error() == Some(65) {
        return Err(err).context(format!(
            "Could not reach {addr} (macOS Local Network permission blocked; enable 'iPhone Mirror' in System Settings -> Privacy & Security -> Local Network)"
        ));
    }
    Err(err).with_context(|| format!("Could not reach {addr}"))
}

/// Connect to a Wi-Fi device and return a ready RSD adapter + handshake.
pub async fn connect(device: &WifiDevice) -> Result<(AdapterHandle, RsdHandshake)> {
    let path = pairing_file_path()?;
    let mut file = load_or_create_pairing_file().await?;

    let pairing_addr = format!("{}:{}", device.host, device.port);
    info!(%pairing_addr, "Connecting to the iPhone over Wi-Fi");
    let stream = connect_tcp_prefer_ipv4(&pairing_addr).await?;
    let socket = RpPairingSocket::new(stream);
    let mut pairing = RemotePairingClient::new(socket, SENDING_HOST);
    pairing
        .connect(&mut file, || async { "000000".to_string() })
        .await
        .context("RemotePairing with the Wi-Fi device failed — connect via USB and click 'Pair Wi-Fi via USB' to refresh pairing keys")?;
    file.write_to_file(&path)
        .await
        .context("Could not persist the RemotePairing file")?;

    let tunnel_port = pairing.create_tcp_listener().await?;
    let tunnel_addr = format!("{}:{}", device.host, tunnel_port);
    let tunnel_stream = connect_tcp_prefer_ipv4(&tunnel_addr).await?;
    let tunnel = connect_tls_psk_tunnel_native(tunnel_stream, pairing.encryption_key())
        .await
        .context("TLS-PSK tunnel handshake failed")?;

    let client_ip = tunnel.info.client_address.parse()?;
    let server_ip = tunnel.info.server_address.parse()?;
    let mtu = tunnel.info.mtu as usize;
    let rsd_port = tunnel.info.server_rsd_port;
    let info = tunnel.info.clone();
    let mut adapter =
        idevice::tcp::adapter::Adapter::new(Box::new(tunnel.into_inner()), client_ip, server_ip);
    adapter.set_mss(mtu.saturating_sub(60).min(8192));
    let mut adapter = adapter.to_async_handle();
    let rsd_stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(rsd_stream).await?;
    info!(?info, "Wi-Fi RSD tunnel established");
    Ok((adapter, handshake))
}

pub fn is_uuid_like(value: &str) -> bool {
    let trimmed = value.trim();
    let hex_only: String = trimmed.chars().filter(|c| *c != '-' && *c != ':').collect();
    hex_only.len() >= 16 && hex_only.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn friendly_name_from_host(host: &str, fallback: &str) -> String {
    let stripped = host
        .trim()
        .trim_end_matches('.')
        .trim_end_matches(".local")
        .trim();
    if !stripped.is_empty() && !is_uuid_like(stripped) {
        stripped.to_string()
    } else if !fallback.is_empty() && !is_uuid_like(fallback) {
        fallback.to_string()
    } else {
        "iPhone".to_string()
    }
}

pub fn parse_avahi_browse_output(output: &str) -> Vec<WifiDevice> {
    let mut devices: Vec<WifiDevice> = Vec::new();
    for line in output.lines() {
        if !line.starts_with("=;") {
            continue;
        }
        let cols: Vec<&str> = line.split(';').collect();
        if cols.len() < 9 {
            continue;
        }
        let raw_name = unescape_avahi_text(cols[3]);
        let hostname = cols[6].trim_end_matches('.').to_string();
        let address = cols[7].trim();
        let Ok(port) = cols[8].trim().parse::<u16>() else {
            continue;
        };
        let host = if !address.is_empty() && cols[2].trim() == "IPv4" {
            address.to_string()
        } else if !hostname.is_empty() {
            hostname.clone()
        } else if !address.is_empty() {
            address.to_string()
        } else {
            continue;
        };

        let txt = cols.get(9).copied().unwrap_or("");
        let identifier = txt
            .split('"')
            .find_map(|token| token.strip_prefix("identifier="))
            .map(str::to_string)
            .unwrap_or_else(|| raw_name.clone());

        let friendly_name = if is_uuid_like(&raw_name) {
            friendly_name_from_host(&hostname, &raw_name)
        } else {
            raw_name
        };

        if let Some(existing) = devices.iter_mut().find(|d| d.identifier == identifier) {
            if cols[2].trim() == "IPv4" {
                existing.host = host;
                existing.port = port;
            }
            continue;
        }
        devices.push(WifiDevice {
            name: friendly_name,
            identifier,
            host,
            port,
        });
    }
    devices
}

fn unescape_avahi_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Some(ch) = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|s| s.parse::<u8>().ok())
            {
                out.push(ch as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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
            let friendly_name = if is_uuid_like(name) {
                friendly_name_from_host(&host, name)
            } else {
                name.to_string()
            };
            return Some(WifiDevice {
                name: friendly_name,
                identifier,
                host,
                port,
            });
        }
    }
    None
}

async fn run_tool(program: &str, args: &[&str], duration: Duration) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Could not run {program}; mDNS discovery is unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("{program} had no stdout"))?;
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
        name: std::env::var("IPHONE_MIRROR_WIFI_NAME").unwrap_or_else(|_| host.clone()),
        identifier: std::env::var("IPHONE_MIRROR_WIFI_IDENTIFIER").unwrap_or_else(|_| host.clone()),
        host,
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_avahi_browse_output, upsert_wifi_device, WifiDevice};

    #[test]
    fn deduplicates_wifi_devices_by_host() {
        let mut devices = Vec::new();
        upsert_wifi_device(
            &mut devices,
            WifiDevice {
                name: "Ar".to_string(),
                identifier: "OLD-UUID".to_string(),
                host: "Ar.local".to_string(),
                port: 49152,
            },
        );
        upsert_wifi_device(
            &mut devices,
            WifiDevice {
                name: "Ar".to_string(),
                identifier: "NEW-UUID".to_string(),
                host: "ar.local.".to_string(),
                port: 49152,
            },
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].identifier, "NEW-UUID");
    }

    #[test]
    fn parses_avahi_remotepairing_resolved_lines() {
        let sample = "\
+;wlan0;IPv4;3AF62AF2-6CCF-4234-B2AE-A6CB63C72C9E;_remotepairing._tcp;local\n\
=;wlan0;IPv4;3AF62AF2-6CCF-4234-B2AE-A6CB63C72C9E;_remotepairing._tcp;local;iPhone-SE.local;192.168.1.42;51234;\"identifier=3AF62AF2-6CCF-4234-B2AE-A6CB63C72C9E\"\n";
        let devices = parse_avahi_browse_output(sample);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "iPhone-SE");
        assert_eq!(
            devices[0].identifier,
            "3AF62AF2-6CCF-4234-B2AE-A6CB63C72C9E"
        );
        assert_eq!(devices[0].host, "192.168.1.42");
        assert_eq!(devices[0].port, 51234);
    }
}

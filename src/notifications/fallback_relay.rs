use crate::notifications::{
    ancs::{
        build_get_app_attributes, build_get_notification_attributes,
        build_perform_notification_action, DataSourceAssembler, DataSourceResponse,
        NotificationAttributeId, NotificationSourceEvent, DEFAULT_NOTIFICATION_ATTRS,
    },
    filter::{FilterDecision, NotificationFilter},
    icon_cache::{fallback_display_name, IconCache},
    ipc::{decode_b64, encode_b64, DaemonToHostMessage, HostToDaemonMessage},
    IosNotification, NotificationCategory, NotificationEventKind, NotificationFlags,
    NotificationSettings,
};
use anyhow::{bail, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

static TEST_UID_SEQ: AtomicU32 = AtomicU32::new(900_000);

#[derive(Debug, Serialize, Deserialize)]
struct RelayNotifyPayload {
    #[serde(default = "default_relay_bundle")]
    app_bundle_id: String,
    #[serde(default)]
    app_display_name: Option<String>,
    title: String,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    silent: bool,
}

/// Best-effort discovery of the laptop's active LAN IPv4 address for HTTP relay instructions.
pub fn local_lan_ip() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let addr = sock.local_addr().ok()?;
    Some(addr.ip().to_string())
}

/// Forwards an iOS notification to another host's `iPhone Mirror` HTTP relay endpoint (`POST /notify`).
pub async fn forward_notification_http(
    url: &str,
    bearer_token: &str,
    notif: &IosNotification,
) -> Result<()> {
    let stripped = url
        .trim()
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("Relay forward URL must start with http://"))?;
    let (host_port, path) = match stripped.find('/') {
        Some(idx) => (&stripped[..idx], &stripped[idx..]),
        None => (stripped, "/notify"),
    };
    let addr = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };

    let category = if notif.category.is_call() {
        Some("incoming_call".to_string())
    } else {
        None
    };
    let payload = RelayNotifyPayload {
        app_bundle_id: notif.app_bundle_id.clone(),
        app_display_name: Some(notif.app_display_name.clone()),
        title: notif.title.clone(),
        subtitle: notif.subtitle.clone(),
        body: notif.body.clone(),
        category,
        silent: notif.flags.silent,
    };
    let json = serde_json::to_vec(&payload)?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nAuthorization: Bearer {bearer_token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        json.len()
    );

    let mut stream = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow::anyhow!("Timed out connecting to relay {addr}"))??;
    stream.write_all(req.as_bytes()).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;

    let mut resp_buf = [0_u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut resp_buf))
        .await
        .map_err(|_| anyhow::anyhow!("Timed out reading relay response"))??;
    let status_line = String::from_utf8_lossy(&resp_buf[..n]);
    if !status_line.contains(" 200 ") && !status_line.contains(" 202 ") {
        bail!("Relay server returned unexpected response: {status_line}");
    }
    Ok(())
}

fn default_relay_bundle() -> String {
    "com.apple.MobileSMS".into()
}

/// Constant-time slice comparison to prevent timing side channels on bearer token verification.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn generate_relay_token() -> String {
    let mut bytes = [0_u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Core stateful notification orchestrator shared by macOS daemon and Linux notification loop.
pub struct NotificationEngine {
    settings: NotificationSettings,
    filter: NotificationFilter,
    icon_cache: IconCache,
    assembler: DataSourceAssembler,
    pending_events: HashMap<u32, NotificationSourceEvent>,
    expected_notif_attrs: HashMap<u32, usize>,
    expected_app_attrs: HashMap<String, usize>,
    mirror_focused: bool,
}

impl NotificationEngine {
    pub fn new(settings: NotificationSettings) -> Result<Self> {
        let icon_cache = IconCache::open_default()?;
        Ok(Self::with_icon_cache(settings, icon_cache))
    }

    pub fn with_icon_cache(settings: NotificationSettings, icon_cache: IconCache) -> Self {
        Self {
            settings,
            filter: NotificationFilter::new(),
            icon_cache,
            assembler: DataSourceAssembler::new(),
            pending_events: HashMap::new(),
            expected_notif_attrs: HashMap::new(),
            expected_app_attrs: HashMap::new(),
            mirror_focused: false,
        }
    }

    pub fn settings(&self) -> &NotificationSettings {
        &self.settings
    }

    pub fn handle_host_message(
        &mut self,
        msg: HostToDaemonMessage,
        now: Instant,
    ) -> Vec<DaemonToHostMessage> {
        match msg {
            HostToDaemonMessage::BleConnected { peripheral_name } => {
                self.filter.mark_session_connected(now);
                self.assembler.reset();
                self.pending_events.clear();
                self.expected_notif_attrs.clear();
                self.expected_app_attrs.clear();
                vec![DaemonToHostMessage::Status {
                    message: format!("ANCS connected to {peripheral_name}"),
                }]
            }
            HostToDaemonMessage::BleDisconnected { reason } => {
                self.assembler.reset();
                self.pending_events.clear();
                self.expected_notif_attrs.clear();
                self.expected_app_attrs.clear();
                vec![DaemonToHostMessage::Status {
                    message: format!("ANCS disconnected ({reason})"),
                }]
            }
            HostToDaemonMessage::BleNotificationSource { bytes_b64 } => {
                let Ok(raw) = decode_b64(&bytes_b64) else {
                    return vec![DaemonToHostMessage::Status {
                        message: "Ignored invalid base64 Notification Source packet".into(),
                    }];
                };
                self.handle_notification_source(&raw, now)
            }
            HostToDaemonMessage::BleDataSource { bytes_b64 } => {
                let Ok(raw) = decode_b64(&bytes_b64) else {
                    return vec![DaemonToHostMessage::Status {
                        message: "Ignored invalid base64 Data Source packet".into(),
                    }];
                };
                self.handle_data_source(&raw, now)
            }
            HostToDaemonMessage::PerformAction { uid, positive } => {
                let cmd = build_perform_notification_action(uid, positive);
                vec![DaemonToHostMessage::WriteControlPoint {
                    bytes_b64: encode_b64(&cmd),
                }]
            }
            HostToDaemonMessage::SetMirrorActive { active } => {
                self.mirror_focused = active;
                Vec::new()
            }
            HostToDaemonMessage::SendTestNotification => {
                let uid = TEST_UID_SEQ.fetch_add(1, Ordering::Relaxed);
                let test = IosNotification {
                    uid,
                    event_kind: NotificationEventKind::Added,
                    flags: NotificationFlags {
                        negative_action: true,
                        ..Default::default()
                    },
                    category: NotificationCategory::Social,
                    app_bundle_id: "com.apple.MobileSMS".into(),
                    app_display_name: self.icon_cache.display_name("com.apple.MobileSMS"),
                    title: "iPhone Mirror Test".into(),
                    subtitle: Some("Native Notification Ready".into()),
                    body: "Notifications from your iPhone will appear natively right here.".into(),
                    date: None,
                    positive_label: Some("Open".into()),
                    negative_label: Some("Dismiss".into()),
                    icon_png_path: self.icon_cache.icon_path_if_exists("com.apple.MobileSMS"),
                };
                vec![DaemonToHostMessage::DeliverNotification { notification: test }]
            }
            HostToDaemonMessage::UpdateSettings { settings } => {
                self.settings = settings;
                Vec::new()
            }
        }
    }

    pub fn handle_notification_source(
        &mut self,
        raw_packet: &[u8],
        now: Instant,
    ) -> Vec<DaemonToHostMessage> {
        let ev = match NotificationSourceEvent::parse(raw_packet) {
            Ok(ev) => ev,
            Err(err) => {
                return vec![DaemonToHostMessage::Status {
                    message: format!("ANCS Notification Source parse error: {err}"),
                }];
            }
        };

        if ev.event_kind == NotificationEventKind::Removed {
            self.pending_events.remove(&ev.uid);
            self.expected_notif_attrs.remove(&ev.uid);
            let dummy = IosNotification {
                uid: ev.uid,
                event_kind: NotificationEventKind::Removed,
                flags: ev.flags,
                category: ev.category,
                app_bundle_id: String::new(),
                app_display_name: String::new(),
                title: String::new(),
                subtitle: None,
                body: String::new(),
                date: None,
                positive_label: None,
                negative_label: None,
                icon_png_path: None,
            };
            return match self
                .filter
                .evaluate(dummy, &self.settings, self.mirror_focused, now)
            {
                FilterDecision::Retract { uid } => {
                    vec![DaemonToHostMessage::RetractNotification { uid }]
                }
                _ => Vec::new(),
            };
        }

        self.pending_events.insert(ev.uid, ev);
        self.expected_notif_attrs
            .insert(ev.uid, DEFAULT_NOTIFICATION_ATTRS.len());
        let cmd = build_get_notification_attributes(ev.uid);
        vec![DaemonToHostMessage::WriteControlPoint {
            bytes_b64: encode_b64(&cmd),
        }]
    }

    pub fn handle_data_source(&mut self, chunk: &[u8], now: Instant) -> Vec<DaemonToHostMessage> {
        let responses = match self.assembler.push_chunk(
            chunk,
            &mut self.expected_notif_attrs,
            &mut self.expected_app_attrs,
        ) {
            Ok(res) => res,
            Err(err) => {
                return vec![DaemonToHostMessage::Status {
                    message: format!("ANCS Data Source parse reset: {err}"),
                }];
            }
        };

        let mut out = Vec::new();
        for resp in responses {
            match resp {
                DataSourceResponse::AppAttributes {
                    app_bundle_id,
                    display_name,
                } => {
                    if let Some(name) = display_name {
                        self.icon_cache.set_display_name(&app_bundle_id, name);
                    }
                }
                DataSourceResponse::NotificationAttributes { uid, attributes } => {
                    let Some(ev) = self.pending_events.remove(&uid) else {
                        continue;
                    };
                    let app_bundle_id = attributes
                        .get(&NotificationAttributeId::AppIdentifier)
                        .cloned()
                        .unwrap_or_default();
                    if !app_bundle_id.is_empty()
                        && !self.icon_cache.has_cached_name(&app_bundle_id)
                        && fallback_display_name(&app_bundle_id) == app_bundle_id
                    {
                        self.expected_app_attrs.insert(app_bundle_id.clone(), 1);
                        let cmd = build_get_app_attributes(&app_bundle_id);
                        out.push(DaemonToHostMessage::WriteControlPoint {
                            bytes_b64: encode_b64(&cmd),
                        });
                    }
                    let app_display_name = self.icon_cache.display_name(&app_bundle_id);
                    let title = attributes
                        .get(&NotificationAttributeId::Title)
                        .cloned()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| app_display_name.clone());
                    let subtitle = attributes
                        .get(&NotificationAttributeId::Subtitle)
                        .cloned()
                        .filter(|s| !s.is_empty());
                    let body = attributes
                        .get(&NotificationAttributeId::Message)
                        .cloned()
                        .unwrap_or_default();
                    let date = attributes
                        .get(&NotificationAttributeId::Date)
                        .cloned()
                        .filter(|s| !s.is_empty());
                    let positive_label = attributes
                        .get(&NotificationAttributeId::PositiveActionLabel)
                        .cloned()
                        .filter(|s| !s.is_empty());
                    let negative_label = attributes
                        .get(&NotificationAttributeId::NegativeActionLabel)
                        .cloned()
                        .filter(|s| !s.is_empty());
                    let icon_png_path = if app_bundle_id.is_empty() {
                        None
                    } else {
                        self.icon_cache.icon_path_if_exists(&app_bundle_id)
                    };

                    let notif = IosNotification {
                        uid,
                        event_kind: ev.event_kind,
                        flags: ev.flags,
                        category: ev.category,
                        app_bundle_id,
                        app_display_name,
                        title,
                        subtitle,
                        body,
                        date,
                        positive_label,
                        negative_label,
                        icon_png_path,
                    };

                    match self
                        .filter
                        .evaluate(notif, &self.settings, self.mirror_focused, now)
                    {
                        FilterDecision::Deliver(n) => {
                            out.push(DaemonToHostMessage::DeliverNotification { notification: n });
                        }
                        FilterDecision::RecordSilently(n) => {
                            out.push(DaemonToHostMessage::RecordHistoryOnly { notification: n });
                        }
                        FilterDecision::Retract { uid } => {
                            out.push(DaemonToHostMessage::RetractNotification { uid });
                        }
                        FilterDecision::Drop => {}
                    }
                }
            }
        }
        out
    }

    pub fn ingest_external_notification(
        &mut self,
        mut notif: IosNotification,
        now: Instant,
    ) -> Vec<DaemonToHostMessage> {
        if notif.icon_png_path.is_none() && !notif.app_bundle_id.is_empty() {
            notif.icon_png_path = self.icon_cache.icon_path_if_exists(&notif.app_bundle_id);
        }
        if notif.app_display_name.is_empty() && !notif.app_bundle_id.is_empty() {
            notif.app_display_name = self.icon_cache.display_name(&notif.app_bundle_id);
        }
        match self
            .filter
            .evaluate(notif, &self.settings, self.mirror_focused, now)
        {
            FilterDecision::Deliver(n) => {
                vec![DaemonToHostMessage::DeliverNotification { notification: n }]
            }
            FilterDecision::RecordSilently(n) => {
                vec![DaemonToHostMessage::RecordHistoryOnly { notification: n }]
            }
            FilterDecision::Retract { uid } => {
                vec![DaemonToHostMessage::RetractNotification { uid }]
            }
            FilterDecision::Drop => Vec::new(),
        }
    }
}

/// Spawns optional localhost/LAN HTTP relay fallback for Hackintosh hosts without working BLE.
pub async fn spawn_relay_server(
    port: u16,
    token: String,
    inbound_tx: mpsc::UnboundedSender<IosNotification>,
) -> Result<()> {
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).await?;
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                break;
            };
            let expected_token = token.clone();
            let tx = inbound_tx.clone();
            tokio::spawn(async move {
                let _ = handle_http_connection(&mut stream, &expected_token, &tx).await;
            });
        }
    });
    Ok(())
}

async fn handle_http_connection(
    stream: &mut tokio::net::TcpStream,
    expected_token: &str,
    inbound_tx: &mpsc::UnboundedSender<IosNotification>,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let mut content_length = 0_usize;
    let mut auth_header = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("authorization") {
                auth_header = v.trim().to_string();
            }
        }
    }

    let provided = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))
        .unwrap_or("");
    if expected_token.is_empty()
        || !constant_time_eq(provided.as_bytes(), expected_token.as_bytes())
    {
        let resp = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\n\r\nUnauthorized";
        reader.into_inner().write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    if method == "GET" && path == "/health" {
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        reader.into_inner().write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    if method == "POST" && path == "/notify" {
        if content_length == 0 || content_length > 64 * 1024 {
            bail!("Invalid Content-Length");
        }
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body).await?;
        let payload: RelayNotifyPayload = serde_json::from_slice(&body)?;
        let uid = TEST_UID_SEQ.fetch_add(1, Ordering::Relaxed);
        let category = match payload.category.as_deref() {
            Some("incoming_call") => NotificationCategory::IncomingCall,
            Some("email") => NotificationCategory::Email,
            Some("schedule") => NotificationCategory::Schedule,
            _ => NotificationCategory::Social,
        };
        let app_display_name = payload
            .app_display_name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| fallback_display_name(&payload.app_bundle_id));
        let notif = IosNotification {
            uid,
            event_kind: NotificationEventKind::Added,
            flags: NotificationFlags {
                silent: payload.silent,
                negative_action: true,
                ..Default::default()
            },
            category,
            app_bundle_id: payload.app_bundle_id,
            app_display_name,
            title: payload.title,
            subtitle: payload.subtitle,
            body: payload.body,
            date: None,
            positive_label: None,
            negative_label: Some("Dismiss".into()),
            icon_png_path: None,
        };
        let _ = inbound_tx.send(notif);
        let resp = "HTTP/1.1 202 Accepted\r\nContent-Length: 8\r\n\r\nAccepted";
        reader.into_inner().write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
    reader.into_inner().write_all(resp.as_bytes()).await?;
    Ok(())
}

/// Runs the stdio JSON-line notification daemon (invoked by `iPhone Mirror.app` or CLI).
pub async fn run_stdio_daemon(initial_settings: NotificationSettings, bluez: bool) -> Result<()> {
    let (relay_tx, mut relay_rx) = mpsc::unbounded_channel();
    let mut settings = initial_settings;
    if settings.relay_token.is_empty() {
        settings.relay_token = generate_relay_token();
    }

    let mut relay_started = false;
    if settings.relay_enabled {
        match spawn_relay_server(
            settings.relay_port,
            settings.relay_token.clone(),
            relay_tx.clone(),
        )
        .await
        {
            Ok(()) => {
                relay_started = true;
            }
            Err(err) => {
                emit_msg(&DaemonToHostMessage::Status {
                    message: format!("Notification relay bind failed: {err}"),
                });
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    let (mut bluez_rx_opt, mut desktop_notifier, mut bluez_cp_tx) = if bluez {
        let (tx, rx) = mpsc::unbounded_channel();
        crate::notifications::linux_bluez::spawn_bluez_ancs_loop(tx);
        (
            Some(rx),
            Some(crate::notifications::linux_desktop::LinuxDesktopNotifier::new()),
            None::<mpsc::UnboundedSender<Vec<u8>>>,
        )
    } else {
        (None, None, None)
    };
    #[cfg(target_os = "macos")]
    let (mut bluez_rx_opt, _bluez_unused) = (None::<()>, bluez);

    let mut engine = NotificationEngine::new(settings.clone())?;
    if relay_started {
        let lan_url =
            local_lan_ip().map(|ip| format!("http://{ip}:{}/notify", settings.relay_port));
        emit_msg(&DaemonToHostMessage::RelayReady {
            port: settings.relay_port,
            token: settings.relay_token.clone(),
            lan_url,
        });
    }

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            res = stdin.read_line(&mut line) => {
                match res {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(mut msg) = serde_json::from_str::<HostToDaemonMessage>(trimmed) {
                            if let HostToDaemonMessage::UpdateSettings { ref mut settings } = msg {
                                if settings.relay_token.is_empty() {
                                    settings.relay_token = engine.settings().relay_token.clone();
                                    if settings.relay_token.is_empty() {
                                        settings.relay_token = generate_relay_token();
                                    }
                                }
                                if settings.relay_enabled && !relay_started {
                                    match spawn_relay_server(
                                        settings.relay_port,
                                        settings.relay_token.clone(),
                                        relay_tx.clone(),
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            relay_started = true;
                                            let lan_url = local_lan_ip().map(|ip| {
                                                format!("http://{ip}:{}/notify", settings.relay_port)
                                            });
                                            emit_msg(&DaemonToHostMessage::RelayReady {
                                                port: settings.relay_port,
                                                token: settings.relay_token.clone(),
                                                lan_url,
                                            });
                                        }
                                        Err(err) => {
                                            emit_msg(&DaemonToHostMessage::Status {
                                                message: format!("Notification relay bind failed: {err}"),
                                            });
                                        }
                                    }
                                }
                            }
                            let now = Instant::now();
                            for out in engine.handle_host_message(msg, now) {
                                emit_msg(&out);
                            }
                        }
                    }
                    Err(err) => bail!("notify-daemon stdin error: {err}"),
                }
            }
            Some(relay_notif) = relay_rx.recv() => {
                let now = Instant::now();
                for out in engine.ingest_external_notification(relay_notif, now) {
                    #[cfg(not(target_os = "macos"))]
                    if let Some(dn) = desktop_notifier.as_mut() {
                        if let DaemonToHostMessage::DeliverNotification { ref notification } = out {
                            dn.deliver(notification);
                        }
                    }
                    emit_msg(&out);
                }
            }
            Some(ev) = next_bluez_event(&mut bluez_rx_opt) => {
                handle_bluez_event_on_daemon(
                    ev,
                    &mut engine,
                    #[cfg(not(target_os = "macos"))]
                    &mut desktop_notifier,
                    #[cfg(not(target_os = "macos"))]
                    &mut bluez_cp_tx,
                );
            }
        }
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn next_bluez_event(
    rx: &mut Option<mpsc::UnboundedReceiver<crate::notifications::linux_bluez::BluezAncsEvent>>,
) -> Option<crate::notifications::linux_bluez::BluezAncsEvent> {
    match rx {
        Some(inner) => inner.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(target_os = "macos")]
async fn next_bluez_event(_rx: &mut Option<()>) -> Option<()> {
    std::future::pending().await
}

#[cfg(target_os = "macos")]
fn handle_bluez_event_on_daemon(_ev: (), _engine: &mut NotificationEngine) {}

#[cfg(not(target_os = "macos"))]
fn handle_bluez_event_on_daemon(
    ev: crate::notifications::linux_bluez::BluezAncsEvent,
    engine: &mut NotificationEngine,
    desktop_notifier: &mut Option<crate::notifications::linux_desktop::LinuxDesktopNotifier>,
    bluez_cp_tx: &mut Option<mpsc::UnboundedSender<Vec<u8>>>,
) {
    use crate::notifications::linux_bluez::BluezAncsEvent;
    let now = Instant::now();
    let outs = match ev {
        BluezAncsEvent::Status(message) => vec![DaemonToHostMessage::Status { message }],
        BluezAncsEvent::Connected { device_name } => engine.handle_host_message(
            HostToDaemonMessage::BleConnected {
                peripheral_name: device_name,
            },
            now,
        ),
        BluezAncsEvent::Disconnected { reason } => {
            *bluez_cp_tx = None;
            engine.handle_host_message(HostToDaemonMessage::BleDisconnected { reason }, now)
        }
        BluezAncsEvent::CommandChannelReady(tx) => {
            *bluez_cp_tx = Some(tx);
            Vec::new()
        }
        BluezAncsEvent::NotificationSourcePacket(raw) => {
            engine.handle_notification_source(&raw, now)
        }
        BluezAncsEvent::DataSourcePacket(raw) => engine.handle_data_source(&raw, now),
    };
    for out in outs {
        match &out {
            DaemonToHostMessage::WriteControlPoint { bytes_b64 } => {
                if let (Some(tx), Ok(bytes)) = (&*bluez_cp_tx, decode_b64(bytes_b64)) {
                    let _ = tx.send(bytes);
                }
            }
            DaemonToHostMessage::DeliverNotification { notification } => {
                if let Some(dn) = desktop_notifier.as_mut() {
                    dn.deliver(notification);
                }
                if !engine.settings().forward_http_url.is_empty() {
                    let url = engine.settings().forward_http_url.clone();
                    let tok = engine.settings().forward_http_token.clone();
                    let n = notification.clone();
                    tokio::spawn(async move {
                        let _ = forward_notification_http(&url, &tok, &n).await;
                    });
                }
            }
            DaemonToHostMessage::RetractNotification { uid } => {
                if let Some(dn) = desktop_notifier.as_mut() {
                    dn.retract(*uid);
                }
            }
            _ => {}
        }
        emit_msg(&out);
    }
}

fn emit_msg(msg: &DaemonToHostMessage) {
    if let Ok(json) = serde_json::to_string(msg) {
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_roundtrips_ancs_source_to_data_to_banner() {
        let tmp = std::env::temp_dir().join(format!("iphone-mirror-test-{}", std::process::id()));
        let cache = IconCache::open_in(tmp.clone()).expect("open temp icon cache");
        let mut engine =
            NotificationEngine::with_icon_cache(NotificationSettings::default(), cache);
        let t0 = Instant::now();

        let _ = engine.handle_host_message(
            HostToDaemonMessage::BleConnected {
                peripheral_name: "Ar iPhone".into(),
            },
            t0,
        );

        // After grace period -> Notification Source triggers GetNotificationAttributes
        let t1 = t0 + std::time::Duration::from_secs(4);
        let src = [0x00, 0x00, 0x04, 0x01, 7, 0, 0, 0];
        let cmds = engine.handle_notification_source(&src, t1);
        assert!(matches!(
            cmds.first(),
            Some(DaemonToHostMessage::WriteControlPoint { .. })
        ));

        // Build matching Data Source payload for UID=7
        let mut payload = vec![0x00, 7, 0, 0, 0];
        let vals: &[(NotificationAttributeId, &[u8])] = &[
            (
                NotificationAttributeId::AppIdentifier,
                b"net.whatsapp.WhatsApp",
            ),
            (NotificationAttributeId::Title, b"Budi"),
            (NotificationAttributeId::Subtitle, b""),
            (NotificationAttributeId::Message, b"Siang!"),
            (NotificationAttributeId::Date, b"20261128T103000"),
            (NotificationAttributeId::PositiveActionLabel, b""),
            (NotificationAttributeId::NegativeActionLabel, b"Clear"),
        ];
        for &(id, val) in vals {
            payload.push(id as u8);
            payload.extend_from_slice(&(val.len() as u16).to_le_bytes());
            payload.extend_from_slice(val);
        }

        let delivered = engine.handle_data_source(&payload, t1);
        assert_eq!(delivered.len(), 1);
        match &delivered[0] {
            DaemonToHostMessage::DeliverNotification { notification } => {
                assert_eq!(notification.app_display_name, "WhatsApp");
                assert_eq!(notification.title, "Budi");
                assert_eq!(notification.body, "Siang!");
            }
            other => panic!("Expected DeliverNotification, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn constant_time_compare_works() {
        assert!(constant_time_eq(b"secret123", b"secret123"));
        assert!(!constant_time_eq(b"secret123", b"secret124"));
        assert!(!constant_time_eq(b"secret", b"secret123"));
    }

    #[tokio::test]
    async fn forward_http_roundtrips_into_relay_server() {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let token = generate_relay_token();
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_relay_server(port, token.clone(), tx)
            .await
            .expect("relay server starts");

        let notif = IosNotification {
            uid: 55,
            event_kind: NotificationEventKind::Added,
            flags: NotificationFlags::default(),
            category: NotificationCategory::Social,
            app_bundle_id: "net.whatsapp.WhatsApp".into(),
            app_display_name: "WhatsApp".into(),
            title: "Ar".into(),
            subtitle: None,
            body: "Forwarded from Linux ANCS".into(),
            date: None,
            positive_label: None,
            negative_label: None,
            icon_png_path: None,
        };
        let url = format!("http://127.0.0.1:{port}/notify");
        forward_notification_http(&url, &token, &notif)
            .await
            .expect("HTTP forward succeeds");

        let received = rx.recv().await.expect("relay received notification");
        assert_eq!(received.title, "Ar");
        assert_eq!(received.body, "Forwarded from Linux ANCS");
        assert_eq!(received.app_bundle_id, "net.whatsapp.WhatsApp");
    }
}

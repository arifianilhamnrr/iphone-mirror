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
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

static TEST_UID_SEQ: AtomicU32 = AtomicU32::new(900_000);

#[derive(Debug, Deserialize)]
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
pub async fn run_stdio_daemon(initial_settings: NotificationSettings) -> Result<()> {
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

    let mut engine = NotificationEngine::new(settings.clone())?;
    if relay_started {
        emit_msg(&DaemonToHostMessage::RelayReady {
            port: settings.relay_port,
            token: settings.relay_token.clone(),
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
                        if let Ok(msg) = serde_json::from_str::<HostToDaemonMessage>(trimmed) {
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
                    emit_msg(&out);
                }
            }
        }
    }

    Ok(())
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
}

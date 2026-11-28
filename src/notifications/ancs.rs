use crate::notifications::{NotificationCategory, NotificationEventKind, NotificationFlags};
use anyhow::{bail, Result};
use std::collections::HashMap;

/// Primary Apple Notification Center Service (ANCS) GATT service UUID.
pub const ANCS_SERVICE_UUID: &str = "7905F431-B5CE-4E99-A40F-4B1E122D00D0";
/// ANCS Notification Source characteristic UUID (notify).
pub const ANCS_NOTIFICATION_SOURCE_UUID: &str = "9FBF120D-6301-42D9-8C58-25E699A21DBD";
/// ANCS Control Point characteristic UUID (write with response).
pub const ANCS_CONTROL_POINT_UUID: &str = "69D1D8F3-45E1-49A8-9821-9BBDFDAAD9D9";
/// ANCS Data Source characteristic UUID (notify).
pub const ANCS_DATA_SOURCE_UUID: &str = "22EAC6E9-24D6-4BB5-BE44-B36ACE7C7BFB";
/// Standard GATT Service Changed characteristic UUID (indicate).
pub const GATT_SERVICE_CHANGED_UUID: &str = "00002A05-0000-1000-8000-00805F9B34FB";

/// ANCS CommandID values.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    GetNotificationAttributes = 0,
    GetAppAttributes = 1,
    PerformNotificationAction = 2,
}

/// ANCS NotificationAttributeID values.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationAttributeId {
    AppIdentifier = 0,
    Title = 1,
    Subtitle = 2,
    Message = 3,
    MessageSize = 4,
    Date = 5,
    PositiveActionLabel = 6,
    NegativeActionLabel = 7,
}

impl NotificationAttributeId {
    pub fn from_raw(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::AppIdentifier),
            1 => Some(Self::Title),
            2 => Some(Self::Subtitle),
            3 => Some(Self::Message),
            4 => Some(Self::MessageSize),
            5 => Some(Self::Date),
            6 => Some(Self::PositiveActionLabel),
            7 => Some(Self::NegativeActionLabel),
            _ => None,
        }
    }

    pub fn has_max_length_param(self) -> bool {
        matches!(self, Self::Title | Self::Subtitle | Self::Message)
    }
}

/// ANCS AppAttributeID values.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppAttributeId {
    DisplayName = 0,
}

impl AppAttributeId {
    pub fn from_raw(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::DisplayName),
            _ => None,
        }
    }
}

/// Parsed 8-byte ANCS Notification Source event packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationSourceEvent {
    pub event_kind: NotificationEventKind,
    pub flags: NotificationFlags,
    pub category: NotificationCategory,
    pub category_count: u8,
    pub uid: u32,
}

impl NotificationSourceEvent {
    pub fn parse(packet: &[u8]) -> Result<Self> {
        if packet.len() != 8 {
            bail!(
                "Invalid ANCS Notification Source packet length {} (expected 8 bytes)",
                packet.len()
            );
        }
        let Some(event_kind) = NotificationEventKind::from_raw(packet[0]) else {
            bail!("Unknown ANCS EventID {}", packet[0]);
        };
        let flags = NotificationFlags::from_raw(packet[1]);
        let category = NotificationCategory::from_raw(packet[2]);
        let category_count = packet[3];
        let uid = u32::from_le_bytes([packet[4], packet[5], packet[6], packet[7]]);
        Ok(Self {
            event_kind,
            flags,
            category,
            category_count,
            uid,
        })
    }
}

/// Attribute list requested for each ANCS notification.
pub const DEFAULT_NOTIFICATION_ATTRS: &[(NotificationAttributeId, Option<u16>)] = &[
    (NotificationAttributeId::AppIdentifier, None),
    (NotificationAttributeId::Title, Some(128)),
    (NotificationAttributeId::Subtitle, Some(128)),
    (NotificationAttributeId::Message, Some(512)),
    (NotificationAttributeId::Date, None),
    (NotificationAttributeId::PositiveActionLabel, None),
    (NotificationAttributeId::NegativeActionLabel, None),
];

/// Builds a `GetNotificationAttributes` (CommandID = 0) Control Point payload.
pub fn build_get_notification_attributes(uid: u32) -> Vec<u8> {
    build_get_notification_attributes_with(uid, DEFAULT_NOTIFICATION_ATTRS)
}

/// Builds a custom `GetNotificationAttributes` Control Point payload.
pub fn build_get_notification_attributes_with(
    uid: u32,
    attrs: &[(NotificationAttributeId, Option<u16>)],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(CommandId::GetNotificationAttributes as u8);
    out.extend_from_slice(&uid.to_le_bytes());
    for &(attr, max_len) in attrs {
        out.push(attr as u8);
        if attr.has_max_length_param() {
            let limit = max_len.unwrap_or(256).max(1);
            out.extend_from_slice(&limit.to_le_bytes());
        }
    }
    out
}

/// Builds a `GetAppAttributes` (CommandID = 1) Control Point payload.
pub fn build_get_app_attributes(app_bundle_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(app_bundle_id.len() + 3);
    out.push(CommandId::GetAppAttributes as u8);
    out.extend_from_slice(app_bundle_id.as_bytes());
    out.push(0); // NUL-terminated AppIdentifier
    out.push(AppAttributeId::DisplayName as u8);
    out
}

/// Builds a `PerformNotificationAction` (CommandID = 2) Control Point payload.
pub fn build_perform_notification_action(uid: u32, positive: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(6);
    out.push(CommandId::PerformNotificationAction as u8);
    out.extend_from_slice(&uid.to_le_bytes());
    out.push(if positive { 0 } else { 1 });
    out
}

/// Completed parsed Data Source response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataSourceResponse {
    NotificationAttributes {
        uid: u32,
        attributes: HashMap<NotificationAttributeId, String>,
    },
    AppAttributes {
        app_bundle_id: String,
        display_name: Option<String>,
    },
}

/// Incremental ANCS Data Source fragment reassembler across arbitrary BLE MTUs.
#[derive(Debug, Default)]
pub struct DataSourceAssembler {
    buf: Vec<u8>,
}

impl DataSourceAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears incomplete buffered fragments (for example on BLE disconnect or timeout).
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Appends a BLE Data Source notification chunk and returns any fully decoded responses.
    pub fn push_chunk(
        &mut self,
        chunk: &[u8],
        expected_notif_attr_counts: &mut HashMap<u32, usize>,
        expected_app_attr_counts: &mut HashMap<String, usize>,
    ) -> Result<Vec<DataSourceResponse>> {
        self.buf.extend_from_slice(chunk);
        let mut responses = Vec::new();

        loop {
            if self.buf.is_empty() {
                break;
            }
            match self.buf[0] {
                0 => {
                    // GetNotificationAttributes response:
                    // [0]=0x00, [1..5]=uid(u32 LE), then N * (attr_id:u8, len:u16 LE, utf8[len])
                    if self.buf.len() < 5 {
                        break;
                    }
                    let uid =
                        u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]);
                    let expected_count = expected_notif_attr_counts
                        .get(&uid)
                        .copied()
                        .unwrap_or(DEFAULT_NOTIFICATION_ATTRS.len());
                    match try_parse_notif_attrs(&self.buf[5..], expected_count)? {
                        Some((consumed, attributes)) => {
                            expected_notif_attr_counts.remove(&uid);
                            self.buf.drain(..5 + consumed);
                            responses.push(DataSourceResponse::NotificationAttributes {
                                uid,
                                attributes,
                            });
                        }
                        None => break,
                    }
                }
                1 => {
                    // GetAppAttributes response:
                    // [0]=0x01, [1..]=app_id\0, then N * (attr_id:u8, len:u16 LE, utf8[len])
                    let Some(nul_rel) = self.buf[1..].iter().position(|&b| b == 0) else {
                        break;
                    };
                    let app_end = 1 + nul_rel;
                    let app_bundle_id = String::from_utf8_lossy(&self.buf[1..app_end]).into_owned();
                    let expected_count = expected_app_attr_counts
                        .get(&app_bundle_id)
                        .copied()
                        .unwrap_or(1);
                    match try_parse_app_attrs(&self.buf[app_end + 1..], expected_count)? {
                        Some((consumed, display_name)) => {
                            expected_app_attr_counts.remove(&app_bundle_id);
                            self.buf.drain(..app_end + 1 + consumed);
                            responses.push(DataSourceResponse::AppAttributes {
                                app_bundle_id,
                                display_name,
                            });
                        }
                        None => break,
                    }
                }
                other => {
                    self.buf.clear();
                    bail!("Unexpected ANCS Data Source CommandID 0x{other:02x}");
                }
            }
        }

        Ok(responses)
    }
}

fn try_parse_notif_attrs(
    payload: &[u8],
    expected_count: usize,
) -> Result<Option<(usize, HashMap<NotificationAttributeId, String>)>> {
    let mut offset = 0;
    let mut attrs = HashMap::with_capacity(expected_count);
    for _ in 0..expected_count {
        if payload.len() < offset + 3 {
            return Ok(None);
        }
        let raw_id = payload[offset];
        let Some(attr_id) = NotificationAttributeId::from_raw(raw_id) else {
            bail!("Unknown ANCS NotificationAttributeID {raw_id}");
        };
        let len = u16::from_le_bytes([payload[offset + 1], payload[offset + 2]]) as usize;
        offset += 3;
        if payload.len() < offset + len {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&payload[offset..offset + len]).into_owned();
        offset += len;
        attrs.insert(attr_id, value);
    }
    Ok(Some((offset, attrs)))
}

fn try_parse_app_attrs(
    payload: &[u8],
    expected_count: usize,
) -> Result<Option<(usize, Option<String>)>> {
    let mut offset = 0;
    let mut display_name = None;
    for _ in 0..expected_count {
        if payload.len() < offset + 3 {
            return Ok(None);
        }
        let raw_id = payload[offset];
        let Some(attr_id) = AppAttributeId::from_raw(raw_id) else {
            bail!("Unknown ANCS AppAttributeID {raw_id}");
        };
        let len = u16::from_le_bytes([payload[offset + 1], payload[offset + 2]]) as usize;
        offset += 3;
        if payload.len() < offset + len {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&payload[offset..offset + len]).into_owned();
        offset += len;
        if attr_id == AppAttributeId::DisplayName && !value.is_empty() {
            display_name = Some(value);
        }
    }
    Ok(Some((offset, display_name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_notification_source_event_and_flags() {
        let raw = [0x00, 0b0001_1101, 0x01, 0x02, 0x78, 0x56, 0x34, 0x12];
        let ev = NotificationSourceEvent::parse(&raw).expect("valid 8-byte packet");
        assert_eq!(ev.event_kind, NotificationEventKind::Added);
        assert!(ev.flags.silent);
        assert!(!ev.flags.important);
        assert!(ev.flags.pre_existing);
        assert!(ev.flags.positive_action);
        assert!(ev.flags.negative_action);
        assert_eq!(ev.category, NotificationCategory::IncomingCall);
        assert_eq!(ev.category_count, 2);
        assert_eq!(ev.uid, 0x1234_5678);
    }

    #[test]
    fn encodes_control_point_commands_with_length_parameters() {
        let cmd = build_get_notification_attributes_with(
            42,
            &[
                (NotificationAttributeId::AppIdentifier, None),
                (NotificationAttributeId::Title, Some(64)),
                (NotificationAttributeId::Message, Some(256)),
            ],
        );
        assert_eq!(
            cmd,
            vec![
                0x00, 42, 0, 0, 0,    // CommandID=0, UID=42 LE
                0x00, // AppIdentifier (no max len)
                0x01, 64, 0, // Title max 64
                0x03, 0, 1, // Message max 256
            ]
        );

        assert_eq!(
            build_get_app_attributes("net.whatsapp.WhatsApp"),
            [&[0x01][..], b"net.whatsapp.WhatsApp", &[0x00, 0x00][..]].concat()
        );
        assert_eq!(
            build_perform_notification_action(0x0102_0304, false),
            vec![0x02, 0x04, 0x03, 0x02, 0x01, 0x01]
        );
    }

    #[test]
    fn reassembles_fragmented_utf8_data_source_across_small_mtu_chunks() {
        let mut full = Vec::new();
        full.push(0x00);
        full.extend_from_slice(&99_u32.to_le_bytes());

        let app = b"net.whatsapp.WhatsApp";
        full.push(NotificationAttributeId::AppIdentifier as u8);
        full.extend_from_slice(&(app.len() as u16).to_le_bytes());
        full.extend_from_slice(app);

        let title = "Budi 🚀".as_bytes();
        full.push(NotificationAttributeId::Title as u8);
        full.extend_from_slice(&(title.len() as u16).to_le_bytes());
        full.extend_from_slice(title);

        let body = "Halo, lagi di mana nih? 👍".as_bytes();
        full.push(NotificationAttributeId::Message as u8);
        full.extend_from_slice(&(body.len() as u16).to_le_bytes());
        full.extend_from_slice(body);

        let mut assembler = DataSourceAssembler::new();
        let mut expected_notif = HashMap::from([(99_u32, 3_usize)]);
        let mut expected_app = HashMap::new();

        let mut decoded = Vec::new();
        // Feed 7-byte slices so multi-byte emoji sequences split mid-codepoint across chunks
        for chunk in full.chunks(7) {
            let out = assembler
                .push_chunk(chunk, &mut expected_notif, &mut expected_app)
                .expect("chunk parse succeeds");
            decoded.extend(out);
        }

        assert_eq!(decoded.len(), 1);
        match &decoded[0] {
            DataSourceResponse::NotificationAttributes { uid, attributes } => {
                assert_eq!(*uid, 99);
                assert_eq!(
                    attributes
                        .get(&NotificationAttributeId::AppIdentifier)
                        .map(String::as_str),
                    Some("net.whatsapp.WhatsApp")
                );
                assert_eq!(
                    attributes
                        .get(&NotificationAttributeId::Title)
                        .map(String::as_str),
                    Some("Budi 🚀")
                );
                assert_eq!(
                    attributes
                        .get(&NotificationAttributeId::Message)
                        .map(String::as_str),
                    Some("Halo, lagi di mana nih? 👍")
                );
            }
            other => panic!("Unexpected response variant: {other:?}"),
        }
    }
}

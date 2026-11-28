pub mod ancs;
pub mod fallback_relay;
pub mod filter;
pub mod icon_cache;
pub mod ipc;

#[cfg(not(target_os = "macos"))]
pub mod linux_bluez;
#[cfg(not(target_os = "macos"))]
pub mod linux_desktop;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// ANCS EventID values (Apple Notification Center Service Specification v1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationEventKind {
    Added,
    Modified,
    Removed,
}

impl NotificationEventKind {
    pub fn from_raw(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Added),
            1 => Some(Self::Modified),
            2 => Some(Self::Removed),
            _ => None,
        }
    }
}

/// ANCS EventFlags bitmask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationFlags {
    pub silent: bool,
    pub important: bool,
    pub pre_existing: bool,
    pub positive_action: bool,
    pub negative_action: bool,
}

impl NotificationFlags {
    pub fn from_raw(bits: u8) -> Self {
        Self {
            silent: (bits & (1 << 0)) != 0,
            important: (bits & (1 << 1)) != 0,
            pre_existing: (bits & (1 << 2)) != 0,
            positive_action: (bits & (1 << 3)) != 0,
            negative_action: (bits & (1 << 4)) != 0,
        }
    }
}

/// ANCS CategoryID values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationCategory {
    Other,
    IncomingCall,
    MissedCall,
    Voicemail,
    Social,
    Schedule,
    Email,
    News,
    HealthAndFitness,
    BusinessAndFinance,
    Location,
    Entertainment,
}

impl NotificationCategory {
    pub fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::IncomingCall,
            2 => Self::MissedCall,
            3 => Self::Voicemail,
            4 => Self::Social,
            5 => Self::Schedule,
            6 => Self::Email,
            7 => Self::News,
            8 => Self::HealthAndFitness,
            9 => Self::BusinessAndFinance,
            10 => Self::Location,
            11 => Self::Entertainment,
            _ => Self::Other,
        }
    }

    pub fn is_call(self) -> bool {
        matches!(self, Self::IncomingCall)
    }
}

/// Normalized iOS notification ready for OS banner delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IosNotification {
    pub uid: u32,
    pub event_kind: NotificationEventKind,
    pub flags: NotificationFlags,
    pub category: NotificationCategory,
    pub app_bundle_id: String,
    pub app_display_name: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    pub body: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub positive_label: Option<String>,
    #[serde(default)]
    pub negative_label: Option<String>,
    #[serde(default)]
    pub icon_png_path: Option<PathBuf>,
}

/// Persistent notification mirroring preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub keep_in_menu_bar: bool,
    #[serde(default = "default_true")]
    pub mute_while_mirroring: bool,
    #[serde(default)]
    pub hide_previews: bool,
    #[serde(default)]
    pub muted_bundle_ids: Vec<String>,
    #[serde(default)]
    pub relay_enabled: bool,
    #[serde(default = "default_relay_port")]
    pub relay_port: u16,
    #[serde(default)]
    pub relay_token: String,
    #[serde(default)]
    pub forward_http_url: String,
    #[serde(default)]
    pub forward_http_token: String,
}

fn default_true() -> bool {
    true
}

fn default_relay_port() -> u16 {
    48125
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_in_menu_bar: true,
            mute_while_mirroring: true,
            hide_previews: false,
            muted_bundle_ids: Vec::new(),
            relay_enabled: false,
            relay_port: default_relay_port(),
            relay_token: String::new(),
            forward_http_url: String::new(),
            forward_http_token: String::new(),
        }
    }
}

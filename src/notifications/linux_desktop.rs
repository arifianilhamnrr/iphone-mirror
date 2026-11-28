use crate::notifications::IosNotification;
use std::collections::HashMap;
use std::process::Command;

/// Delivers native Linux desktop notifications via FreeDesktop D-Bus / `notify-send`.
pub struct LinuxDesktopNotifier {
    delivered_ids: HashMap<u32, u32>,
}

impl Default for LinuxDesktopNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxDesktopNotifier {
    pub fn new() -> Self {
        Self {
            delivered_ids: HashMap::new(),
        }
    }

    pub fn deliver(&mut self, notif: &IosNotification) {
        if let Some(dbus_id) = deliver_via_gdbus(notif) {
            self.delivered_ids.insert(notif.uid, dbus_id);
            return;
        }
        deliver_via_notify_send(notif);
    }

    pub fn retract(&mut self, ancs_uid: u32) {
        if let Some(dbus_id) = self.delivered_ids.remove(&ancs_uid) {
            let _ = Command::new("gdbus")
                .args([
                    "call",
                    "--session",
                    "--dest",
                    "org.freedesktop.Notifications",
                    "--object-path",
                    "/org/freedesktop/Notifications",
                    "--method",
                    "org.freedesktop.Notifications.CloseNotification",
                    &dbus_id.to_string(),
                ])
                .output();
        }
    }
}

pub fn format_summary(notif: &IosNotification) -> String {
    if notif.title.eq_ignore_ascii_case(&notif.app_display_name) || notif.title.is_empty() {
        notif.app_display_name.clone()
    } else {
        format!("{} • {}", notif.app_display_name, notif.title)
    }
}

pub fn format_body(notif: &IosNotification) -> String {
    match &notif.subtitle {
        Some(sub) if !sub.is_empty() => {
            if notif.body.is_empty() {
                sub.clone()
            } else {
                format!("{sub}\n{}", notif.body)
            }
        }
        _ => notif.body.clone(),
    }
}

fn deliver_via_gdbus(notif: &IosNotification) -> Option<u32> {
    let summary = format_summary(notif);
    let body = format_body(notif);
    let icon = notif
        .icon_png_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "phone".to_string());
    let urgency: u8 = if notif.category.is_call() || notif.flags.important {
        2
    } else {
        1
    };
    let hints = format!("{{'urgency': <byte {urgency}>}}");

    let out = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "iPhone Mirror",
            "0",
            &icon,
            &summary,
            &body,
            "[]",
            &hints,
            "5000",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `gdbus` returns e.g. `(uint32 42,)\n`
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok())
}

fn deliver_via_notify_send(notif: &IosNotification) {
    let summary = format_summary(notif);
    let body = format_body(notif);
    let urgency = if notif.category.is_call() || notif.flags.important {
        "critical"
    } else {
        "normal"
    };
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "iPhone Mirror", "-u", urgency]);
    if let Some(icon) = &notif.icon_png_path {
        cmd.args(["-i", &icon.to_string_lossy()]);
    }
    cmd.args([&summary, &body]);
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::{NotificationCategory, NotificationEventKind, NotificationFlags};

    #[test]
    fn formats_desktop_summary_and_body_cleanly() {
        let n = IosNotification {
            uid: 1,
            event_kind: NotificationEventKind::Added,
            flags: NotificationFlags::default(),
            category: NotificationCategory::Social,
            app_bundle_id: "net.whatsapp.WhatsApp".into(),
            app_display_name: "WhatsApp".into(),
            title: "Budi".into(),
            subtitle: Some("Grup Kantor".into()),
            body: "Meeting jam 2".into(),
            date: None,
            positive_label: None,
            negative_label: None,
            icon_png_path: None,
        };
        assert_eq!(format_summary(&n), "WhatsApp • Budi");
        assert_eq!(format_body(&n), "Grup Kantor\nMeeting jam 2");
    }
}

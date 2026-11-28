use crate::notifications::{IosNotification, NotificationEventKind, NotificationSettings};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const INITIAL_SYNC_GRACE_WINDOW: Duration = Duration::from_millis(2500);
const DUPLICATE_SUPPRESS_WINDOW: Duration = Duration::from_secs(3);

/// Disposition of an incoming notification after storm-guard, privacy, and mute filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterDecision {
    /// Deliver as a native OS banner/alert.
    Deliver(IosNotification),
    /// Record silently in in-app history without popping a desktop banner.
    RecordSilently(IosNotification),
    /// Retract a previously delivered notification from the OS notification center.
    Retract { uid: u32 },
    /// Drop without displaying (disabled or muted/duplicate).
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecentSignature {
    app_bundle_id: String,
    title: String,
    body: String,
    seen_at: Instant,
}

/// Stateful notification pipeline filter.
pub struct NotificationFilter {
    connected_at: Option<Instant>,
    recent_by_uid: HashMap<u32, RecentSignature>,
}

impl Default for NotificationFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationFilter {
    pub fn new() -> Self {
        Self {
            connected_at: None,
            recent_by_uid: HashMap::new(),
        }
    }

    /// Marks the timestamp when BLE ANCS session started so initial pre-existing sync never floods banners.
    pub fn mark_session_connected(&mut self, now: Instant) {
        self.connected_at = Some(now);
    }

    pub fn clear(&mut self) {
        self.connected_at = None;
        self.recent_by_uid.clear();
    }

    pub fn evaluate(
        &mut self,
        mut notif: IosNotification,
        settings: &NotificationSettings,
        mirror_focused: bool,
        now: Instant,
    ) -> FilterDecision {
        if !settings.enabled {
            return FilterDecision::Drop;
        }

        if notif.event_kind == NotificationEventKind::Removed {
            self.recent_by_uid.remove(&notif.uid);
            return FilterDecision::Retract { uid: notif.uid };
        }

        let is_muted_app = settings
            .muted_bundle_ids
            .iter()
            .any(|id| id.eq_ignore_ascii_case(&notif.app_bundle_id));
        if is_muted_app {
            return FilterDecision::Drop;
        }

        // Deduplicate rapid repeated ANCS Modified events with identical text.
        if let Some(prev) = self.recent_by_uid.get(&notif.uid) {
            if notif.event_kind == NotificationEventKind::Modified
                && prev.app_bundle_id == notif.app_bundle_id
                && prev.title == notif.title
                && prev.body == notif.body
                && now.duration_since(prev.seen_at) < DUPLICATE_SUPPRESS_WINDOW
            {
                return FilterDecision::Drop;
            }
        }
        self.recent_by_uid.insert(
            notif.uid,
            RecentSignature {
                app_bundle_id: notif.app_bundle_id.clone(),
                title: notif.title.clone(),
                body: notif.body.clone(),
                seen_at: now,
            },
        );
        if self.recent_by_uid.len() > 256 {
            self.recent_by_uid
                .retain(|_, entry| now.duration_since(entry.seen_at) < Duration::from_secs(300));
        }

        if settings.hide_previews {
            notif.title = notif.app_display_name.clone();
            notif.subtitle = None;
            notif.body = format!("1 new notification from {}", notif.app_display_name);
        }

        let in_grace_window = self
            .connected_at
            .map(|t| now.saturating_duration_since(t) < INITIAL_SYNC_GRACE_WINDOW)
            .unwrap_or(false);

        if notif.flags.pre_existing || in_grace_window {
            return FilterDecision::RecordSilently(notif);
        }

        if mirror_focused && settings.mute_while_mirroring && !notif.category.is_call() {
            return FilterDecision::RecordSilently(notif);
        }

        FilterDecision::Deliver(notif)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::{NotificationCategory, NotificationFlags};

    fn sample_notif(uid: u32, pre_existing: bool) -> IosNotification {
        IosNotification {
            uid,
            event_kind: NotificationEventKind::Added,
            flags: NotificationFlags {
                pre_existing,
                ..Default::default()
            },
            category: NotificationCategory::Social,
            app_bundle_id: "net.whatsapp.WhatsApp".into(),
            app_display_name: "WhatsApp".into(),
            title: "Ar".into(),
            subtitle: None,
            body: "OTP: 123456".into(),
            date: None,
            positive_label: None,
            negative_label: Some("Clear".into()),
            icon_png_path: None,
        }
    }

    #[test]
    fn suppresses_pre_existing_storm_and_grace_window() {
        let mut filter = NotificationFilter::new();
        let start = Instant::now();
        filter.mark_session_connected(start);
        let settings = NotificationSettings::default();

        // Within initial 2.5s sync grace -> recorded silently
        let d1 = filter.evaluate(sample_notif(1, true), &settings, false, start);
        assert!(matches!(d1, FilterDecision::RecordSilently(_)));

        let d2 = filter.evaluate(
            sample_notif(2, false),
            &settings,
            false,
            start + Duration::from_millis(500),
        );
        assert!(matches!(d2, FilterDecision::RecordSilently(_)));

        // After grace window and non-pre-existing -> delivered as native banner
        let d3 = filter.evaluate(
            sample_notif(3, false),
            &settings,
            false,
            start + Duration::from_secs(4),
        );
        assert!(matches!(d3, FilterDecision::Deliver(_)));
    }

    #[test]
    fn redacts_previews_and_respects_muted_apps() {
        let mut filter = NotificationFilter::new();
        let now = Instant::now() + Duration::from_secs(10);
        let mut settings = NotificationSettings {
            hide_previews: true,
            muted_bundle_ids: vec!["com.muted.app".into()],
            ..Default::default()
        };

        let mut muted = sample_notif(10, false);
        muted.app_bundle_id = "com.muted.app".into();
        assert_eq!(
            filter.evaluate(muted, &settings, false, now),
            FilterDecision::Drop
        );

        let delivered = filter.evaluate(sample_notif(11, false), &settings, false, now);
        match delivered {
            FilterDecision::Deliver(n) => {
                assert_eq!(n.body, "1 new notification from WhatsApp");
                assert!(!n.body.contains("123456"));
            }
            other => panic!("Expected Deliver, got {other:?}"),
        }

        // Foreground mirror mutes regular notifications but not incoming calls
        settings.hide_previews = false;
        let d_mirror = filter.evaluate(sample_notif(12, false), &settings, true, now);
        assert!(matches!(d_mirror, FilterDecision::RecordSilently(_)));

        let mut call = sample_notif(13, false);
        call.category = NotificationCategory::IncomingCall;
        let d_call = filter.evaluate(call, &settings, true, now);
        assert!(matches!(d_call, FilterDecision::Deliver(_)));
    }
}

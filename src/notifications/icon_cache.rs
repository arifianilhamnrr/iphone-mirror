use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Returns OS-appropriate persistent cache directory for iPhone app icons and metadata.
pub fn cache_root_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join("Library/Caches/id.web.projectar.iphone-mirror/icons"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(xdg)
        } else {
            let home = env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".cache")
        };
        Ok(base.join("iphone-mirror").join("icons"))
    }
}

/// Sanitizes an iOS bundle identifier into a safe single filename component.
pub fn sanitized_icon_filename(bundle_id: &str) -> String {
    let mut name = String::with_capacity(bundle_id.len() + 4);
    for ch in bundle_id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            name.push(ch);
        } else {
            name.push('_');
        }
    }
    if name.is_empty() {
        name.push_str("unknown");
    }
    name.push_str(".png");
    name
}

/// Resolves known iOS built-in and common third-party bundle IDs to friendly display names.
pub fn fallback_display_name(bundle_id: &str) -> String {
    match bundle_id {
        "com.apple.MobileSMS" => "Messages".into(),
        "com.apple.mobilephone" => "Phone".into(),
        "com.apple.facetime" => "FaceTime".into(),
        "com.apple.mobilemail" => "Mail".into(),
        "com.apple.mobilecal" => "Calendar".into(),
        "com.apple.reminders" => "Reminders".into(),
        "com.apple.mobilenotes" => "Notes".into(),
        "com.apple.Preferences" => "Settings".into(),
        "com.apple.AppStore" => "App Store".into(),
        "com.apple.weather" => "Weather".into(),
        "com.apple.Health" => "Health".into(),
        "com.apple.Passbook" => "Wallet".into(),
        "net.whatsapp.WhatsApp" => "WhatsApp".into(),
        "net.whatsapp.WhatsAppSMB" => "WhatsApp Business".into(),
        "ph.telegra.Telegraph" => "Telegram".into(),
        "com.burbn.instagram" => "Instagram".into(),
        "com.atebits.Tweetie2" => "X".into(),
        "com.facebook.Facebook" => "Facebook".into(),
        "com.facebook.Messenger" => "Messenger".into(),
        "com.google.Gmail" => "Gmail".into(),
        "com.google.ios.youtube" => "YouTube".into(),
        "com.zhiliaoapp.musically" => "TikTok".into(),
        "com.discord.Discord" => "Discord".into(),
        "com.tinyspeck.chatlyio" => "Slack".into(),
        "com.shopee.id" => "Shopee".into(),
        "id.bni.wondr" => "wondr by BNI".into(),
        _ => bundle_id
            .rsplit('.')
            .next()
            .filter(|tail| !tail.is_empty())
            .unwrap_or(bundle_id)
            .to_string(),
    }
}

/// Local icon & app name cache backed by disk PNGs and `names.json`.
pub struct IconCache {
    root: PathBuf,
    names: BTreeMap<String, String>,
}

impl IconCache {
    pub fn open_default() -> Result<Self> {
        Self::open_in(cache_root_dir()?)
    }

    pub fn open_in(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).with_context(|| {
            format!(
                "Failed to create icon cache directory at {}",
                root.display()
            )
        })?;
        let names = fs::read(root.join("names.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Ok(Self { root, names })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn icon_path_if_exists(&self, bundle_id: &str) -> Option<PathBuf> {
        let path = self.root.join(sanitized_icon_filename(bundle_id));
        if path.is_file() {
            Some(path)
        } else {
            None
        }
    }

    pub fn store_icon_png(&self, bundle_id: &str, png_bytes: &[u8]) -> Result<PathBuf> {
        // Basic PNG signature guard before writing cache files.
        if !png_bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            anyhow::bail!("Refusing to cache non-PNG icon payload for {bundle_id}");
        }
        let path = self.root.join(sanitized_icon_filename(bundle_id));
        fs::write(&path, png_bytes)?;
        Ok(path)
    }

    pub fn display_name(&self, bundle_id: &str) -> String {
        self.names
            .get(bundle_id)
            .cloned()
            .unwrap_or_else(|| fallback_display_name(bundle_id))
    }

    pub fn has_cached_name(&self, bundle_id: &str) -> bool {
        self.names.contains_key(bundle_id)
    }

    pub fn set_display_name(&mut self, bundle_id: &str, name: String) {
        if name.trim().is_empty() {
            return;
        }
        self.names.insert(bundle_id.to_string(), name);
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.names) {
            let _ = fs::write(self.root.join("names.json"), bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_bundle_ids_and_resolves_fallback_names() {
        assert_eq!(
            sanitized_icon_filename("net.whatsapp.WhatsApp"),
            "net.whatsapp.WhatsApp.png"
        );
        assert_eq!(
            sanitized_icon_filename("../../etc/passwd"),
            ".._.._etc_passwd.png"
        );
        assert_eq!(fallback_display_name("com.apple.MobileSMS"), "Messages");
        assert_eq!(
            fallback_display_name("com.example.CustomMessenger"),
            "CustomMessenger"
        );
    }
}

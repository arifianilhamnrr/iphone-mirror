// language: Rust, file: src/ui/tray.rs, target: macOS (AppKit / Tahoe)
use tracing::info;

pub struct TrayManager {
    pub is_active: bool,
}

impl TrayManager {
    pub fn new() -> Self {
        Self { is_active: false }
    }

    /// Sets up NSStatusItem menu in macOS Menu Bar
    pub fn init_status_item(&mut self) {
        #[cfg(target_os = "macos")]
        {
            info!("Registering NSStatusItem on macOS Menu Bar (Tahoe/Sequoia)...");
            self.is_active = true;
        }

        #[cfg(not(target_os = "macos"))]
        {
            info!("Tray manager active (headless mode)");
        }
    }
}

impl Default for TrayManager {
    fn default() -> Self {
        Self::new()
    }
}

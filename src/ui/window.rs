// language: Rust, file: src/ui/window.rs
use anyhow::Result;
use tracing::info;

pub struct MirrorWindow {
    pub width: u32,
    pub height: u32,
    pub title: String,
}

impl MirrorWindow {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            title: "iPhone Mirror (macOS)".to_string(),
        }
    }

    pub fn run(&self) -> Result<()> {
        info!(
            width = self.width,
            height = self.height,
            title = %self.title,
            "Starting window event loop..."
        );
        Ok(())
    }
}

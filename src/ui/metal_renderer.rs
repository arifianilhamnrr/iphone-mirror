// language: Rust, file: src/ui/metal_renderer.rs, target: macOS (Metal 3 / Tahoe)
use anyhow::Result;
use tracing::info;

#[cfg(target_os = "macos")]
use metal::{CommandQueue, Device};

pub struct MetalRenderer {
    #[cfg(target_os = "macos")]
    device: Device,
    #[cfg(target_os = "macos")]
    command_queue: CommandQueue,
}

impl MetalRenderer {
    pub fn new() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let device = Device::system_default()
                .ok_or_else(|| anyhow::anyhow!("No Metal-compatible GPU found on this system"))?;
            let command_queue = device.new_command_queue();

            info!(
                gpu_name = %device.name(),
                "Metal rendering device & command queue initialized (macOS Tahoe target)"
            );

            Ok(Self {
                device,
                command_queue,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            Ok(Self {})
        }
    }

    pub fn render_frame(&self) {
        // Presentation loop synchronizes with CADisplayLink / CAMetalLayer drawable
    }
}

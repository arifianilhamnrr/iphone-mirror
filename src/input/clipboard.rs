// language: Rust, file: src/input/clipboard.rs, target: macOS
use anyhow::{bail, Result};

pub struct SystemClipboard {
    #[cfg(target_os = "macos")]
    pasteboard: objc2::rc::Retained<objc2_app_kit::NSPasteboard>,
    change_count: isize,
}

impl SystemClipboard {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            let pasteboard = unsafe { objc2_app_kit::NSPasteboard::generalPasteboard() };
            let change_count = unsafe { pasteboard.changeCount() };
            Self {
                pasteboard,
                change_count,
            }
        }
        #[cfg(not(target_os = "macos"))]
        Self { change_count: 0 }
    }

    pub fn changed_text(&mut self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            let change_count = unsafe { self.pasteboard.changeCount() };
            if change_count == self.change_count {
                return None;
            }
            self.change_count = change_count;
            unsafe {
                self.pasteboard
                    .stringForType(objc2_app_kit::NSPasteboardTypeString)
                    .map(|text| text.to_string())
            }
        }
        #[cfg(not(target_os = "macos"))]
        None
    }

    pub fn current_text(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        unsafe {
            self.pasteboard
                .stringForType(objc2_app_kit::NSPasteboardTypeString)
                .map(|text| text.to_string())
        }
        #[cfg(not(target_os = "macos"))]
        None
    }

    pub fn set_text(&mut self, text: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let text = objc2_foundation::NSString::from_str(text);
            unsafe {
                self.pasteboard.clearContents();
                if !self
                    .pasteboard
                    .setString_forType(&text, objc2_app_kit::NSPasteboardTypeString)
                {
                    bail!("NSPasteboard rejected the clipboard text");
                }
                self.change_count = self.pasteboard.changeCount();
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = text;
            bail!("System clipboard synchronization is only supported on macOS")
        }
    }
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

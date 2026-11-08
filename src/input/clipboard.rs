// language: Rust, file: src/input/clipboard.rs, target: macOS
use anyhow::{bail, Result};

pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum ClipboardContent {
    Text(String),
    Image { uti: String, bytes: Vec<u8> },
}

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

    pub fn changed_content(&mut self) -> Option<ClipboardContent> {
        #[cfg(target_os = "macos")]
        {
            let change_count = unsafe { self.pasteboard.changeCount() };
            if change_count == self.change_count {
                return None;
            }
            self.change_count = change_count;
            unsafe {
                for (pasteboard_type, uti) in [
                    (objc2_app_kit::NSPasteboardTypePNG, "public.png"),
                    (objc2_app_kit::NSPasteboardTypeTIFF, "public.tiff"),
                ] {
                    if let Some(data) = self.pasteboard.dataForType(pasteboard_type) {
                        let length = data.length();
                        if length == 0 || length > MAX_CLIPBOARD_IMAGE_BYTES {
                            continue;
                        }
                        let mut bytes = vec![0_u8; length];
                        let destination = std::ptr::NonNull::new(bytes.as_mut_ptr().cast())
                            .expect("non-empty clipboard image buffer");
                        data.getBytes_length(destination, length);
                        return Some(ClipboardContent::Image {
                            uti: uti.to_string(),
                            bytes,
                        });
                    }
                }
                self.pasteboard
                    .stringForType(objc2_app_kit::NSPasteboardTypeString)
                    .map(|text| ClipboardContent::Text(text.to_string()))
            }
        }
        #[cfg(not(target_os = "macos"))]
        None
    }

    pub fn set_image(&mut self, uti: &str, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            bail!("Refusing to set an empty clipboard image");
        }
        if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
            bail!(
                "Clipboard image is {} bytes; the limit is {} bytes",
                bytes.len(),
                MAX_CLIPBOARD_IMAGE_BYTES
            );
        }
        #[cfg(target_os = "macos")]
        {
            let pasteboard_type = objc2_foundation::NSString::from_str(uti);
            let data = unsafe {
                objc2_foundation::NSData::dataWithBytes_length(
                    bytes.as_ptr().cast_mut().cast(),
                    bytes.len(),
                )
            };
            unsafe {
                self.pasteboard.clearContents();
                if !self
                    .pasteboard
                    .setData_forType(Some(&data), &pasteboard_type)
                {
                    bail!("NSPasteboard rejected the clipboard image");
                }
                self.change_count = self.pasteboard.changeCount();
            }
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (uti, bytes);
            bail!("System clipboard synchronization is only supported on macOS")
        }
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

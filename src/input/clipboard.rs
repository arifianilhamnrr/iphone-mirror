// language: Rust, file: src/input/clipboard.rs, target: macOS & Linux (Wayland/X11)
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
    #[cfg(target_os = "macos")]
    change_count: isize,
    #[cfg(not(target_os = "macos"))]
    last_text: Option<String>,
    #[cfg(not(target_os = "macos"))]
    latest_text: std::sync::Arc<std::sync::Mutex<Option<String>>>,
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
        {
            let initial = linux_read_text();
            let latest_text = std::sync::Arc::new(std::sync::Mutex::new(initial.clone()));
            let watcher_state = latest_text.clone();
            let _ = std::thread::Builder::new()
                .name("linux-clipboard-watcher".to_string())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    let current = linux_read_text();
                    if let Ok(mut guard) = watcher_state.lock() {
                        *guard = current;
                    }
                });
            Self {
                last_text: initial,
                latest_text,
            }
        }
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
        {
            let current = self.latest_text.try_lock().ok()?.clone()?;
            if self.last_text.as_deref() == Some(current.as_str()) {
                return None;
            }
            self.last_text = Some(current.clone());
            Some(ClipboardContent::Text(current))
        }
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
            let mime = if uti.contains("png") {
                "image/png"
            } else if uti.contains("jpeg") {
                "image/jpeg"
            } else {
                "image/png"
            };
            linux_write_bytes(mime, bytes)
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
        linux_read_text()
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
            linux_write_bytes("text/plain", text.as_bytes())?;
            self.last_text = Some(text.to_string());
            Ok(())
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

#[cfg(not(target_os = "macos"))]
fn linux_read_text() -> Option<String> {
    use std::process::{Command, Stdio};
    if is_wayland() {
        if let Ok(out) = Command::new("wl-paste")
            .args(["--no-newline", "--type", "text/plain"])
            .stderr(Stdio::null())
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).into_owned();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    for (bin, args) in [
        ("xclip", &["-selection", "clipboard", "-o"][..]),
        ("xsel", &["--clipboard", "--output"][..]),
    ] {
        if let Ok(out) = Command::new(bin).args(args).stderr(Stdio::null()).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).into_owned();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn linux_write_bytes(mime: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let candidates: Vec<(&str, Vec<&str>)> = if is_wayland() {
        vec![
            ("wl-copy", vec!["--type", mime]),
            ("xclip", vec!["-selection", "clipboard", "-t", mime, "-i"]),
        ]
    } else if mime == "text/plain" {
        vec![
            ("xclip", vec!["-selection", "clipboard", "-i"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ]
    } else {
        vec![("xclip", vec!["-selection", "clipboard", "-t", mime, "-i"])]
    };

    for (bin, args) in candidates {
        if let Ok(mut child) = Command::new(bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(bytes);
            }
            if let Ok(status) = child.wait() {
                if status.success() {
                    return Ok(());
                }
            }
        }
    }
    bail!(
        "No Linux clipboard utility found (install wl-clipboard for Wayland or xclip/xsel for X11)"
    )
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

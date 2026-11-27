// language: Rust, file: src/transfer/metadata.rs
use super::error::{Result, TransferError};
use std::path::PathBuf;

pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_BATCH_BYTES: u64 = 250 * 1024 * 1024;
pub const MAX_FILES: usize = 10;
pub const MAX_NAME_BYTES: usize = 255;

#[derive(Debug, Clone)]
pub struct ValidatedFile {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

/// Sanitize an untrusted file name into one safe to create on the device.
pub fn sanitize_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':' | '\0'))
        .map(|c| if c == '\r' || c == '\n' { '_' } else { c })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        out = "file".to_string();
    }
    while out.len() > MAX_NAME_BYTES {
        out.pop();
    }
    out
}

/// Validate a batch of dropped paths and return a normalized file list.
pub fn validate_paths(paths: &[PathBuf]) -> Result<Vec<ValidatedFile>> {
    if paths.len() > MAX_FILES {
        return Err(TransferError::TooManyFiles {
            count: paths.len(),
            limit: MAX_FILES,
        });
    }

    let mut result = Vec::with_capacity(paths.len());
    let mut used = std::collections::HashSet::new();
    let mut total = 0_u64;

    for path in paths {
        let metadata = std::fs::symlink_metadata(path).map_err(TransferError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(TransferError::Symlink(path.display().to_string()));
        }
        if metadata.is_dir() {
            return Err(TransferError::Directory(path.display().to_string()));
        }
        let size = metadata.len();
        if size > MAX_FILE_BYTES {
            return Err(TransferError::FileTooLarge {
                size,
                limit: MAX_FILE_BYTES,
            });
        }
        total = total.saturating_add(size);
        if total > MAX_BATCH_BYTES {
            return Err(TransferError::BatchTooLarge {
                size: total,
                limit: MAX_BATCH_BYTES,
            });
        }

        let base = sanitize_file_name(path.file_name().and_then(|n| n.to_str()).unwrap_or("file"));
        let name = unique_name(&base, &mut used);
        result.push(ValidatedFile {
            path: path.clone(),
            name,
            size,
        });
    }
    Ok(result)
}

fn unique_name(base: &str, used: &mut std::collections::HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (base, String::new()),
    };
    for index in 2..10_000 {
        let candidate = format!("{stem} ({index}){ext}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    format!("{stem}-{}{ext}", uuid::Uuid::new_v4())
}

/// Remote staging root used for transferred files.
///
/// AFC's namespace is rooted at `/var/mobile/Media`, so `/Downloads` here maps
/// to the real path `/var/mobile/Media/Downloads` that iOS apps can resolve.
pub fn staging_root() -> &'static str {
    "/Downloads/iphone-mirror-transfers"
}

/// Absolute on-device path that must be used in a `file://` URL.
pub fn device_path(remote_path: &str) -> String {
    if let Some(suffix) = remote_path.strip_prefix('/') {
        format!("/var/mobile/Media/{suffix}")
    } else {
        format!("/var/mobile/Media/{remote_path}")
    }
}

pub fn remote_path(name: &str) -> String {
    format!("{}/{}", staging_root(), name)
}

pub fn file_url(remote_path: &str) -> String {
    let encoded = percent_encode_path(&device_path(remote_path));
    format!("file://{encoded}")
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_name("a/b:c"), "abc");
        assert_eq!(sanitize_file_name(""), "file");
        assert_eq!(sanitize_file_name(".."), "file");
    }

    #[test]
    fn unique_names() {
        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_name("a.txt", &mut used), "a.txt");
        assert_eq!(unique_name("a.txt", &mut used), "a (2).txt");
        assert_eq!(unique_name("a.txt", &mut used), "a (3).txt");
    }

    #[test]
    fn file_url_format() {
        assert_eq!(
            file_url("/tmp/a b.txt"),
            "file:///var/mobile/Media/tmp/a%20b.txt"
        );
    }
}

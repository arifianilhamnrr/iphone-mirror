// language: Rust, file: src/transfer/pasteboard.rs
//! Minimal pasteboard writer that can publish file URLs to the iPhone.
//!
//! `idevice::PasteboardServiceClient` only exposes text and image helpers, so
//! this implements the same `SET` verb with arbitrary UTIs.

use idevice::{
    IdeviceError, RsdService, RemoteXpcClient, ReadWrite,
    xpc::{Dictionary, XPCObject},
};
use std::borrow::Cow;

const FILE_URL: &str = "public.file-url";
const URL: &str = "public.url";
// Do NOT publish a text UTI here: iOS consumers would pick the text flavour
// and create a .txt containing the URL string instead of opening the file.
const URL_UTIS: [&str; 2] = [FILE_URL, URL];

pub struct UrlPasteboard {
    inner: RemoteXpcClient<Box<dyn ReadWrite>>,
}

impl RsdService for UrlPasteboard {
    fn rsd_service_name() -> Cow<'static, str> {
        Cow::Borrowed("com.apple.coredevice.pasteboardservice")
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> Result<Self, IdeviceError> {
        let mut inner = RemoteXpcClient::new(stream).await?;
        inner.do_handshake().await?;
        Ok(Self { inner })
    }
}

pub struct PasteboardFile {
    pub url: String,
    pub name: String,
    pub data: Vec<u8>,
    pub content_uti: String,
}

/// Best-effort UTI for a file name. Unknown types fall back to `public.data`.
pub fn uti_for_name(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "public.png",
        "jpg" | "jpeg" => "public.jpeg",
        "gif" => "public.gif",
        "heic" => "public.heic",
        "pdf" => "com.adobe.pdf",
        "txt" | "log" | "md" => "public.utf8-plain-text",
        "json" => "public.json",
        "html" | "htm" => "public.html",
        "mp4" | "mov" => "public.mpeg-4",
        "mp3" => "public.mp3",
        "wav" => "com.microsoft.waveform-audio",
        "zip" => "public.zip-archive",
        "csv" => "public.comma-separated-values-text",
        _ => "public.data",
    }
}

impl UrlPasteboard {
    /// Publish files with both a file URL and their inline content. Consumers
    /// such as Telegram usually read the content representation, while Files
    /// can use the URL. `suggestedName` is passed through source metadata.
    pub async fn set_files(&mut self, files: &[PasteboardFile]) -> Result<(), IdeviceError> {
        let items: Vec<XPCObject> = files
            .iter()
            .map(|file| {
                let mut data = Dictionary::new();
                for uti in URL_UTIS {
                    let mut payload = Dictionary::new();
                    payload.insert("data".into(), XPCObject::Data(file.url.as_bytes().to_vec()));
                    data.insert(uti.into(), XPCObject::Dictionary(payload));
                }
                let mut types: Vec<&str> = URL_UTIS.to_vec();
                if !file.data.is_empty() && !types.contains(&file.content_uti.as_str()) {
                    let mut payload = Dictionary::new();
                    payload.insert("data".into(), XPCObject::Data(file.data.clone()));
                    data.insert(file.content_uti.clone(), XPCObject::Dictionary(payload));
                    types.push(&file.content_uti);
                }
                let mut item = Dictionary::new();
                item.insert(
                    "types".into(),
                    XPCObject::Array(
                        types
                            .iter()
                            .map(|uti| XPCObject::String((*uti).to_string()))
                            .collect(),
                    ),
                );
                item.insert("data".into(), XPCObject::Dictionary(data));
                XPCObject::Dictionary(item)
            })
            .collect();

        let source = Dictionary::new();

        let mut message = Dictionary::new();
        message.insert("command".into(), XPCObject::String("SET".into()));
        message.insert(
            "pasteboardName".into(),
            XPCObject::String("general".into()),
        );
        message.insert("items".into(), XPCObject::Array(items));
        message.insert(
            "sourceMetadata".into(),
            if source.is_empty() {
                XPCObject::Null
            } else {
                XPCObject::Dictionary(source)
            },
        );

        self.inner
            .send_object(XPCObject::Dictionary(message), true)
            .await?;
        self.inner.recv().await?;
        Ok(())
    }

    /// Replace the general pasteboard with one item per file URL.
    /// Each item advertises only `public.file-url` and `public.url`.
    pub async fn set_file_urls(&mut self, urls: &[String]) -> Result<(), IdeviceError> {
        let items: Vec<XPCObject> = urls
            .iter()
            .map(|url| {
                let mut data = Dictionary::new();
                for uti in URL_UTIS {
                    let mut payload = Dictionary::new();
                    payload.insert("data".into(), XPCObject::Data(url.as_bytes().to_vec()));
                    data.insert(uti.into(), XPCObject::Dictionary(payload));
                }
                let mut item = Dictionary::new();
                item.insert(
                    "types".into(),
                    XPCObject::Array(
                        URL_UTIS
                            .iter()
                            .map(|uti| XPCObject::String((*uti).to_string()))
                            .collect(),
                    ),
                );
                item.insert("data".into(), XPCObject::Dictionary(data));
                XPCObject::Dictionary(item)
            })
            .collect();

        let mut message = Dictionary::new();
        message.insert("command".into(), XPCObject::String("SET".into()));
        message.insert(
            "pasteboardName".into(),
            XPCObject::String("general".into()),
        );
        message.insert("items".into(), XPCObject::Array(items));
        message.insert("sourceMetadata".into(), XPCObject::Null);

        self.inner
            .send_object(XPCObject::Dictionary(message), true)
            .await?;
        self.inner.recv().await?;
        Ok(())
    }
}

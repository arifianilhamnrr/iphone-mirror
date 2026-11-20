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
const UTF8_TEXT: &str = "public.utf8-plain-text";
const URL_UTIS: [&str; 3] = [FILE_URL, URL, UTF8_TEXT];

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

impl UrlPasteboard {
    /// Replace the general pasteboard with one item per file URL.
    /// Each item advertises `public.file-url`, `public.url`, and plain text.
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

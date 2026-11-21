// language: Rust, file: src/transfer/manager.rs
use super::{
    afc::AfcTransfer,
    error::{Result, TransferError},
    metadata::{MAX_FILE_BYTES, file_url, validate_paths},
    pasteboard::{PasteboardFile, UrlPasteboard, uti_for_name},
};
use idevice::RsdService;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::info;

#[derive(Debug)]
pub enum TransferCommand {
    SendFiles(Vec<PathBuf>),
    Cleanup,
}

#[derive(Debug, Clone)]
pub enum TransferEvent {
    Started { files: usize },
    Progress {
        name: String,
        file_index: usize,
        file_count: usize,
        sent: u64,
        total: u64,
    },
    FileStaged {
        name: String,
        remote_path: String,
        url: String,
    },
    Published { files: usize },
    Completed { files: usize },
    Failed { name: Option<String>, message: String },
}

pub struct TransferManager {
    commands: mpsc::UnboundedSender<TransferCommand>,
    events: mpsc::UnboundedReceiver<TransferEvent>,
}

impl TransferManager {
    pub async fn spawn(
        mut afc: AfcTransfer,
        provider: &mut impl idevice::provider::RsdProvider,
        handshake: &mut idevice::rsd::RsdHandshake,
        paste_request: mpsc::UnboundedSender<()>,
    ) -> Result<Self> {
        let pasteboard = UrlPasteboard::connect_rsd(provider, handshake)
            .await
            .map_err(TransferError::Device)?;
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut pasteboard = pasteboard;
            while let Some(command) = command_rx.recv().await {
                match command {
                    TransferCommand::SendFiles(paths) => {
                        if let Err(error) =
                            send_batch(&mut afc, &mut pasteboard, &event_tx, &paste_request, paths)
                                .await
                        {
                            let _ = event_tx.send(TransferEvent::Failed {
                                name: None,
                                message: error.to_string(),
                            });
                        }
                    }
                    TransferCommand::Cleanup => {
                        for path in afc.list_staging().await.unwrap_or_default() {
                            let full = format!(
                                "{}/{}",
                                super::metadata::staging_root(),
                                path.trim_start_matches('/')
                            );
                            let _ = afc.remove_file(&full).await;
                        }
                    }
                }
            }
        });

        Ok(Self {
            commands: command_tx,
            events: event_rx,
        })
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<TransferCommand> {
        self.commands.clone()
    }

    pub async fn next_event(&mut self) -> Option<TransferEvent> {
        self.events.recv().await
    }
}

fn auto_paste_enabled() -> bool {
    !matches!(
        std::env::var("IPHONE_MIRROR_AUTO_PASTE").as_deref(),
        Ok("0") | Ok("false")
    )
}

async fn send_batch(
    afc: &mut AfcTransfer,
    pasteboard: &mut UrlPasteboard,
    events: &mpsc::UnboundedSender<TransferEvent>,
    paste_request: &mpsc::UnboundedSender<()>,
    paths: Vec<PathBuf>,
) -> Result<()> {
    let files = validate_paths(&paths)?;
    let file_count = files.len();
    let _ = events.send(TransferEvent::Started { files: file_count });

    // Match the per-file limit: the pasteboard can usually hold larger inline
    // payloads than the old conservative 20 MiB cap assumed.
    let inline_limit = MAX_FILE_BYTES as usize;
    let mut pasted = Vec::with_capacity(file_count);
    for (index, file) in files.iter().enumerate() {
        let name = file.name.clone();
        let remote = afc
            .upload_file(&file.path, &name, |sent, total| {
                let _ = events.send(TransferEvent::Progress {
                    name: name.clone(),
                    file_index: index + 1,
                    file_count,
                    sent,
                    total,
                });
            })
            .await?;

        let url = file_url(&remote);
        let bytes = tokio::fs::read(&file.path).await.unwrap_or_default();
        let data = if bytes.len() <= inline_limit { bytes } else { Vec::new() };
        pasted.push(PasteboardFile {
            url: url.clone(),
            name: name.clone(),
            data,
            content_uti: uti_for_name(&name).to_string(),
        });
        let _ = events.send(TransferEvent::FileStaged {
            name,
            remote_path: remote,
            url,
        });
        info!(file_index = index + 1, file_count, "staged transfer file");
    }

    if let Err(error) = pasteboard.set_files(&pasted).await {
        tracing::warn!(%error, "inline pasteboard payload rejected; retrying with URLs only");
        for file in &mut pasted {
            file.data.clear();
        }
        pasteboard
            .set_files(&pasted)
            .await
            .map_err(TransferError::Device)?;
    }
    // Give the daemon a moment to commit the SET before the owning task (and
    // its XPC connection) goes away.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = events.send(TransferEvent::Published { files: file_count });
    // Simulate Cmd+V so a drop lands directly in the focused iPhone app.
    if auto_paste_enabled() {
        let _ = paste_request.send(());
    }
    let _ = events.send(TransferEvent::Completed { files: file_count });
    Ok(())
}

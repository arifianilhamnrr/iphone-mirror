// language: Rust, file: src/transfer/afc.rs
//! AFC-based file staging over the existing CoreDevice tunnel.

use super::{
    error::{Result, TransferError},
    metadata::{remote_path, staging_root},
};
use idevice::{
    RsdService,
    afc::{AfcClient, opcode::AfcFopenMode},
    provider::RsdProvider,
    rsd::RsdHandshake,
};
use std::path::Path;
use tokio::io::AsyncReadExt;
use tracing::{debug, info};

const CHUNK_SIZE: usize = 128 * 1024;

pub struct AfcTransfer {
    client: AfcClient,
}

impl AfcTransfer {
    pub async fn connect(
        provider: &mut impl RsdProvider,
        handshake: &mut RsdHandshake,
    ) -> Result<Self> {
        let client = AfcClient::connect_rsd(provider, handshake)
            .await
            .map_err(|_| TransferError::AfcUnavailable)?;
        Ok(Self { client })
    }

    /// Stream a local file to the device, reporting progress after each chunk.
    pub async fn upload_file<F>(
        &mut self,
        local: &Path,
        name: &str,
        mut progress: F,
    ) -> Result<String>
    where
        F: FnMut(u64, u64),
    {
        let destination = remote_path(name);
        let _ = self.client.mk_dir(staging_root()).await;

        let mut source = tokio::fs::File::open(local).await?;
        let total = source.metadata().await?.len();

        let mut handle = self.client.open(&destination, AfcFopenMode::WrOnly).await?;
        let mut buffer = vec![0_u8; CHUNK_SIZE];
        let mut sent = 0_u64;
        loop {
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            handle.write_entire(&buffer[..read]).await?;
            sent += read as u64;
            progress(sent, total);
            tokio::task::yield_now().await;
        }
        handle.close().await?;

        let info = self.client.get_file_info(&destination).await?;
        if info.size as u64 != total {
            return Err(TransferError::Other(anyhow::anyhow!(
                "uploaded size mismatch for {name}: device reports {} bytes, expected {total}",
                info.size
            )));
        }
        info!(%destination, size = total, "staged file on device");
        Ok(destination)
    }

    /// Read a staged file back, used by the Phase 0 round-trip probe.
    pub async fn read_file(&mut self, remote: &str) -> Result<Vec<u8>> {
        let mut handle = self.client.open(remote, AfcFopenMode::RdOnly).await?;
        let bytes = handle.read_entire().await?;
        handle.close().await?;
        Ok(bytes)
    }

    pub async fn remove_file(&mut self, remote: &str) -> Result<()> {
        self.client.remove(remote).await?;
        debug!(%remote, "removed staged file");
        Ok(())
    }

    pub async fn list_staging(&mut self) -> Result<Vec<String>> {
        match self.client.list_dir(staging_root()).await {
            Ok(list) => Ok(list),
            Err(idevice::IdeviceError::Afc(..)) => Ok(Vec::new()),
            Err(error) => Err(TransferError::Device(error)),
        }
    }
}

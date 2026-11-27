// language: Rust, file: src/transport/ddi_mounter.rs
use anyhow::{bail, Context, Result};
use idevice::{
    http::get,
    provider::RsdProvider,
    rsd::RsdHandshake,
    services::{
        cryptexd::{assets::Cryptex1Assets, install_ddi, installed_ddi, CryptexdClient},
        mobile_image_mounter::ImageMounter,
    },
    RsdService,
};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Build of the Cryptex1 DeveloperDiskImage that is cached and installed when
/// the device is missing `com.apple.coredevice.displayservice`.
///
/// DDI-provided RSD services are declared by launch daemons inside the image.
/// The Xcode 26 images (17...) do not declare the display service; the 27A...
/// train and newer do. Keep this pinned to a build that includes it.
const DDI_BUILD: &str = "27B52";

const DDI_BASE_URL: &str = concat!(
    "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/",
    "PersonalizedImages/Xcode_iOS_DDI_Cryptex"
);

const MANIFEST_FILE: &str = "BuildManifest.plist";
const ASSET_FILES: [&str; 4] = [
    "Image.dmg",
    "Image.dmg.trustcache",
    "Image.dmg.cryptex_info",
    "Image.dmg.root_hash",
];

pub struct DdiMounter;

impl DdiMounter {
    /// Standard Xcode and developer cache paths on macOS
    pub fn default_developer_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(home) = std::env::var("HOME") {
            paths.push(PathBuf::from(&home).join("Library/Developer/DeveloperDiskImages"));
            paths.push(PathBuf::from(&home).join(".pymobiledevice3/image_mounter"));
        }
        paths.push(PathBuf::from(
            "/Applications/Xcode.app/Contents/Developer/Platforms/iPhoneOS.platform/DeviceSupport",
        ));
        paths
    }

    pub fn find_ddi_image(version: &str) -> Option<(PathBuf, PathBuf)> {
        for base in Self::default_developer_paths() {
            let img_path = base.join(format!("{}/DeveloperDiskImage.dmg", version));
            let sig_path = base.join(format!("{}/DeveloperDiskImage.dmg.signature", version));
            if img_path.exists() && sig_path.exists() {
                return Some((img_path, sig_path));
            }
        }
        None
    }

    /// Directory used to cache the downloaded Cryptex1 DDI payloads.
    pub fn cache_dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        let base = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("Library/Application Support/iPhone Mirror");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(".cache/iphone-mirror");
        base.join("DDI")
    }

    /// Download the DDI payloads into the local cache without touching the device.
    pub async fn warm_cache() -> Result<()> {
        load_cryptex_assets().await.map(|_| ())
    }

    /// Install a DeveloperDiskImage that provides the display service when the
    /// device currently lacks it. Returns `true` when a DDI was installed.
    pub async fn ensure_display_service(
        provider: &mut impl RsdProvider,
        handshake: &mut RsdHandshake,
    ) -> Result<bool> {
        let assets = match load_cryptex_assets().await {
            Ok(assets) => assets,
            Err(error) => {
                warn!(%error, "Could not prepare the DeveloperDiskImage for automatic mounting");
                return Ok(false);
            }
        };

        // A stale cryptex stays installed after a failed unmount, and cryptexd
        // then refuses to install the replacement as "already mounted".
        if let Some(current) = installed_ddi(provider, handshake).await? {
            info!(
                identifier = %current.identifier,
                version = %current.version,
                "Removing the currently installed DeveloperDiskImage cryptex"
            );
            let client: CryptexdClient<Box<dyn idevice::ReadWrite>> =
                CryptexdClient::connect_rsd(provider, handshake).await?;
            if let Err(error) = client
                .uninstall(&current.identifier, Some(&current.version))
                .await
            {
                warn!(%error, "Could not uninstall the previous DeveloperDiskImage; continuing");
            }
        }

        // Xcode can stage a DDI at /System/Developer without registering it as
        // a cryptex. cryptexd then refuses to create the same mount path, so
        // unmount it through the image mounter first.
        let mut mounter = match ImageMounter::connect_rsd(provider, handshake).await {
            Ok(client) => client,
            Err(error) => {
                warn!(%error, "Could not reach the image mounter to clear the old DDI");
                return Ok(false);
            }
        };
        match mounter.copy_devices().await {
            Ok(devices) => debug!(?devices, "Image mounter devices before DDI install"),
            Err(error) => warn!(%error, "Could not list mounted images"),
        }
        for mount_path in ["/System/Developer", "/Developer"] {
            match mounter.unmount_image(mount_path).await {
                Ok(()) => info!(mount_path, "Unmounted the previous DeveloperDiskImage"),
                Err(error) => {
                    debug!(%error, mount_path, "No image mounted at path")
                }
            }
            // Reconnect: some image mounter failures close the socket.
            mounter = match ImageMounter::connect_rsd(provider, handshake).await {
                Ok(client) => client,
                Err(error) => {
                    warn!(%error, "Could not reconnect to the image mounter");
                    break;
                }
            };
        }

        let installed = install_ddi(provider, handshake, &assets)
            .await
            .context("Failed to install the DeveloperDiskImage cryptex")?;
        info!(
            identifier = %installed.identifier,
            version = %installed.version,
            "DeveloperDiskImage cryptex installed"
        );
        Ok(true)
    }
}

/// Load the cached Cryptex1 assets, downloading them from the
/// `doronz88/DeveloperDiskImage` repository when missing or outdated.
async fn load_cryptex_assets() -> Result<Cryptex1Assets> {
    let dir = DdiMounter::cache_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("Could not create {}", dir.display()))?;

    if cached_build_is_stale(&dir).await {
        info!(build = DDI_BUILD, "Downloading a fresh DeveloperDiskImage");
        for file in std::iter::once(MANIFEST_FILE).chain(ASSET_FILES.iter().copied()) {
            let _ = tokio::fs::remove_file(dir.join(file)).await;
        }
    }

    let manifest_bytes = fetch_asset(&dir, MANIFEST_FILE).await?;
    let manifest: plist::Dictionary = plist::from_bytes(&manifest_bytes)
        .context("Could not parse the cached DeveloperDiskImage manifest")?;
    let build_identity = idevice::tss::select_cryptex_build_identity(&manifest)
        .context("Could not find a Cryptex1 build identity in the DDI manifest")?
        .clone();

    let mut payloads = Vec::with_capacity(ASSET_FILES.len());
    for file in ASSET_FILES {
        payloads.push(fetch_asset(&dir, file).await?);
    }
    let mut payloads = payloads.into_iter();
    let image = payloads.next().expect("four assets read above");
    let trustcache = payloads.next().expect("four assets read above");
    let info = payloads.next().expect("four assets read above");
    let volumehash = payloads.next().expect("four assets read above");

    Ok(Cryptex1Assets::from_parts(
        image,
        trustcache,
        info,
        volumehash,
        build_identity,
    ))
}

async fn cached_build_is_stale(dir: &Path) -> bool {
    let Ok(bytes) = tokio::fs::read(dir.join(MANIFEST_FILE)).await else {
        return true;
    };
    let Ok(value) = plist::from_bytes::<plist::Value>(&bytes) else {
        return true;
    };
    let Some(dict) = value.into_dictionary() else {
        return true;
    };
    let build = dict
        .get("ProductBuildVersion")
        .and_then(|value| value.as_string());
    if build != Some(DDI_BUILD) {
        info!(
            cached = ?build,
            expected = DDI_BUILD,
            "Cached DeveloperDiskImage build does not match; refreshing"
        );
        return true;
    }
    ASSET_FILES.iter().any(|file| !dir.join(file).is_file())
}

async fn fetch_asset(dir: &Path, name: &str) -> Result<Vec<u8>> {
    let path = dir.join(name);
    if let Ok(bytes) = tokio::fs::read(&path).await {
        if !bytes.is_empty() {
            return Ok(bytes);
        }
    }

    let url = format!("{DDI_BASE_URL}/{name}");
    info!(%url, "Downloading DeveloperDiskImage asset");
    let response = get(&url)
        .await
        .with_context(|| format!("Failed to download {url}"))?;
    if response.status != 200 {
        bail!("Downloading {url} returned HTTP {}", response.status);
    }
    if response.body.is_empty() {
        bail!("Downloading {url} returned an empty body");
    }
    tokio::fs::write(&path, &response.body)
        .await
        .with_context(|| format!("Could not cache {}", path.display()))?;
    Ok(response.body)
}

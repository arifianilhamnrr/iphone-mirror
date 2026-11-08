// language: Rust, file: src/protocol/display_client.rs
use crate::transport::rsd_tunnel::RsdTunnel;
use anyhow::{Context, Result};
use idevice::{
    core_device::{
        build_screen_audio_offer, build_screen_video_offer, build_start_audio_parameters,
        build_start_video_parameters, CallInfoBlob, DisplayServiceClient, MediaNegotiationBlob,
    },
    tcp::handle::UdpSocketHandle,
    ReadWrite, RsdService,
};
use tracing::info;
use uuid::Uuid;

const CLIENT_SUPPORTED_FEATURES: u64 = 140;
const SERVICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub struct DisplayClient {
    client: DisplayServiceClient<Box<dyn ReadWrite>>,
    audio_udp: UdpSocketHandle,
    video_udp: UdpSocketHandle,
}

impl DisplayClient {
    /// Connect to displayservice and negotiate the audio+video session expected by Device Hub.
    pub async fn start(tunnel: &mut RsdTunnel, display_id: i64) -> Result<Self> {
        let mut client = tokio::time::timeout(
            SERVICE_TIMEOUT,
            DisplayServiceClient::connect_rsd(&mut tunnel.adapter, &mut tunnel.handshake),
        )
        .await
        .context("Timed out connecting to displayservice")?
        .context("Failed to connect to displayservice")?;

        let audio_udp = tunnel
            .adapter
            .bind_udp(0)
            .await
            .context("Failed to bind the tunneled audio receiver")?;
        let video_udp = tunnel
            .adapter
            .bind_udp(0)
            .await
            .context("Failed to bind the tunneled video receiver")?;

        let receiver_ip = tunnel.adapter.host_ip().to_string();
        let sender_ip = tunnel.adapter.peer_ip().to_string();
        let session_id = Uuid::new_v4();
        let call_info = host_call_info();

        let audio_call_id = Uuid::new_v4().to_string().to_uppercase();
        let audio_offer = build_screen_audio_offer(&audio_call_id, &call_info)
            .context("Failed to build the audio negotiation offer")?;
        let audio_params = build_start_audio_parameters(
            &receiver_ip,
            audio_udp.local_port(),
            &sender_ip,
            50_000,
            audio_offer,
            CLIENT_SUPPORTED_FEATURES,
            session_id,
        );
        let audio_answer =
            tokio::time::timeout(SERVICE_TIMEOUT, client.start_media_stream(audio_params))
                .await
                .context("Timed out starting the audio media stream")?
                .context("Display service rejected the audio media stream")?;
        dump_answer("audio", &audio_answer);
        log_audio_negotiation(&audio_answer);

        let video_call_id = Uuid::new_v4().to_string().to_uppercase();
        let controller_ssrc = Uuid::new_v4().as_u128() as u32;
        let video_offer = build_screen_video_offer(&video_call_id, &call_info, controller_ssrc)
            .context("Failed to build the video negotiation offer")?;
        let video_params = build_start_video_parameters(
            &receiver_ip,
            video_udp.local_port(),
            &sender_ip,
            50_001,
            video_offer,
            CLIENT_SUPPORTED_FEATURES,
            display_id,
            session_id,
        );

        let video_answer =
            match tokio::time::timeout(SERVICE_TIMEOUT, client.start_media_stream(video_params))
                .await
                .context("Timed out starting the video media stream")?
            {
                Ok(answer) => answer,
                Err(error) => {
                    let _ = client.stop_media_stream().await;
                    return Err(error).context("Display service rejected the video media stream");
                }
            };
        dump_answer("video", &video_answer);

        info!(
            %session_id,
            receiver_ip,
            receiver_port = video_udp.local_port(),
            "Display stream negotiated"
        );

        Ok(Self {
            client,
            audio_udp,
            video_udp,
        })
    }

    pub async fn recv_audio_packet(&self) -> Result<Vec<u8>> {
        Ok(self
            .audio_udp
            .recv()
            .await
            .context("Tunneled audio receiver stopped")?
            .data)
    }

    pub async fn recv_video_packet(&self) -> Result<Vec<u8>> {
        Ok(self
            .video_udp
            .recv()
            .await
            .context("Tunneled video receiver stopped")?
            .data)
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.client
            .stop_media_stream()
            .await
            .context("Failed to stop the display media stream")?;
        Ok(())
    }
}

fn dump_answer(kind: &str, answer: &plist::Value) {
    let Ok(directory) = std::env::var("IPHONE_MIRROR_DUMP_NEGOTIATION") else {
        return;
    };
    let path = std::path::Path::new(&directory).join(format!("{kind}-answer.plist"));
    if let Ok(mut file) = std::fs::File::create(&path) {
        let _ = plist::to_writer_xml(&mut file, answer);
        info!(path = %path.display(), "Dumped media negotiation answer");
    }
}

fn log_audio_negotiation(answer: &plist::Value) {
    let Some(blob) = answer
        .as_dictionary()
        .and_then(|dict| dict.get("negotiatorAnswer"))
        .and_then(plist::Value::as_data)
    else {
        return;
    };
    let compressed = plist::from_bytes::<plist::Value>(blob)
        .ok()
        .and_then(|value| {
            value
                .as_dictionary()?
                .get("avcMediaStreamNegotiatorMediaBlob")?
                .as_data()
                .map(Vec::from)
        });
    let Some(compressed) = compressed else {
        tracing::warn!("Audio negotiation answer did not contain a media blob");
        return;
    };
    match MediaNegotiationBlob::from_negotiator_offer(&compressed) {
        Ok(negotiation) => info!(?negotiation, "Parsed audio negotiation"),
        Err(error) => tracing::warn!(%error, "Could not parse audio negotiation answer"),
    }
}

fn host_call_info() -> CallInfoBlob {
    CallInfoBlob {
        call_id: 0,
        client_version: 1,
        device_type: "MacPro7,1".to_string(),
        framework_version: "2205.3.1".to_string(),
        os_version: "26.0".to_string(),
        device_name: Some("iphone-mirror-rs".to_string()),
        audio_device_uid: None,
    }
}

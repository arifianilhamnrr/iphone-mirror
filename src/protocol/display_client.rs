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

#[derive(Debug, Clone, Copy)]
pub struct RtcpSession {
    /// SSRC the device expects in our RTCP sender field.
    local_ssrc: u32,
    /// SSRC carried by the device's RTP stream.
    remote_ssrc: u32,
    /// Port from the negotiation answer where RTCP reports must be delivered.
    rtcp_port: u16,
}

pub struct DisplayClient {
    client: DisplayServiceClient<Box<dyn ReadWrite>>,
    audio_udp: UdpSocketHandle,
    audio_session: Option<RtcpSession>,
    video_udp: UdpSocketHandle,
    video_session: Option<RtcpSession>,
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

        let (audio_udp, audio_session, video_udp, video_session) =
            Self::negotiate(&mut client, tunnel, display_id).await?;

        Ok(Self {
            client,
            audio_udp,
            audio_session,
            video_udp,
            video_session,
        })
    }

    /// Stop and renegotiate a stalled media session. This makes the device
    /// start a fresh encoder stream with new parameter sets and an IDR frame,
    /// which is the only reliable recovery from a burst of lost video packets.
    pub async fn restart(&mut self, tunnel: &mut RsdTunnel, display_id: i64) -> Result<()> {
        let _ = tokio::time::timeout(SERVICE_TIMEOUT, self.client.stop_media_stream()).await;
        let (audio_udp, audio_session, video_udp, video_session) =
            Self::negotiate(&mut self.client, tunnel, display_id).await?;
        self.audio_udp = audio_udp;
        self.audio_session = audio_session;
        self.video_udp = video_udp;
        self.video_session = video_session;
        Ok(())
    }

    async fn negotiate(
        client: &mut DisplayServiceClient<Box<dyn ReadWrite>>,
        tunnel: &mut RsdTunnel,
        display_id: i64,
    ) -> Result<(UdpSocketHandle, Option<RtcpSession>, UdpSocketHandle, Option<RtcpSession>)> {
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
        let audio_session = rtcp_session_from_answer(&audio_answer);
        if audio_session.is_none() {
            tracing::warn!("Audio streamConfig is missing SSRC/RTCP fields; keepalive disabled");
        }

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
        let video_session = rtcp_session_from_answer(&video_answer);
        if video_session.is_none() {
            tracing::warn!("Video streamConfig is missing SSRC/RTCP fields; loss recovery disabled");
        }

        info!(
            %session_id,
            receiver_ip,
            receiver_port = video_udp.local_port(),
            "Display stream negotiated"
        );

        Ok((audio_udp, audio_session, video_udp, video_session))
    }

    /// Build a compound RTCP Receiver Report + SDES. The device uses these to
    /// keep both media streams alive (RTCPTimeoutInterval).
    fn receiver_report(
        session: &RtcpSession,
        highest_sequence: u32,
        stats: &super::rtcp_stats::RtcpReportStats,
    ) -> Vec<u8> {
        let mut packet = Vec::with_capacity(48);
        packet.extend_from_slice(&[
            0x81, 0xC9, 0x00, 0x07, // V=2, RC=1, PT=RR, length=7 words
        ]);
        packet.extend_from_slice(&session.local_ssrc.to_be_bytes());
        packet.extend_from_slice(&session.remote_ssrc.to_be_bytes());
        let cumulative = stats.cumulative_lost.clamp(0, 0x00FF_FFFF) as u32;
        let lost_word = ((stats.fraction_lost as u32) << 24) | cumulative;
        packet.extend_from_slice(&lost_word.to_be_bytes());
        packet.extend_from_slice(&highest_sequence.to_be_bytes());
        packet.extend_from_slice(&stats.jitter.to_be_bytes());
        packet.extend_from_slice(&stats.lsr.to_be_bytes());
        packet.extend_from_slice(&stats.dlsr.to_be_bytes());
        packet.extend_from_slice(&[
            0x81, 0xCA, 0x00, 0x02, // V=2, SC=1, PT=SDES, length=2 words
        ]);
        packet.extend_from_slice(&session.local_ssrc.to_be_bytes());
        packet.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // empty CNAME + END
        packet
    }

    pub fn audio_rtcp_report(
        &self,
        stats: &super::rtcp_stats::RtcpReportStats,
    ) -> Option<Vec<u8>> {
        self.audio_session
            .map(|session| Self::receiver_report(&session, stats.extended_highest(), stats))
    }

    pub fn video_rtcp_report(
        &self,
        stats: &super::rtcp_stats::RtcpReportStats,
    ) -> Option<Vec<u8>> {
        self.video_session
            .map(|session| Self::receiver_report(&session, stats.extended_highest(), stats))
    }

    /// RTCP Picture Loss Indication (RFC 4585, PT=206/FMT=1). Without this the
    /// iPhone's encoder never resends a keyframe after RTP loss and the decoded
    /// picture stays frozen forever.
    pub fn video_pli(&self) -> Option<Vec<u8>> {
        let session = self.video_session?;
        let mut packet = Vec::with_capacity(12);
        packet.extend_from_slice(&[
            0x81, 0xCE, 0x00, 0x02, // V=2, FMT=1, PT=PSFB, length=2 words
        ]);
        packet.extend_from_slice(&session.local_ssrc.to_be_bytes());
        packet.extend_from_slice(&session.remote_ssrc.to_be_bytes());
        Some(packet)
    }

    pub async fn send_audio_rtcp(&self, packet: Vec<u8>) -> Result<()> {
        let Some(session) = self.audio_session else {
            return Ok(());
        };
        self.audio_udp
            .send_to(session.rtcp_port, packet)
            .await
            .context("Failed to send the audio RTCP receiver report")?;
        Ok(())
    }

    pub async fn send_video_rtcp(&self, packet: Vec<u8>) -> Result<()> {
        let Some(session) = self.video_session else {
            return Ok(());
        };
        self.video_udp
            .send_to(session.rtcp_port, packet)
            .await
            .context("Failed to send the video RTCP feedback")?;
        Ok(())
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

fn rtcp_session_from_answer(answer: &plist::Value) -> Option<RtcpSession> {
    let config = answer
        .as_dictionary()?
        .get("connection")?
        .as_dictionary()?
        .get("streamConfig")?
        .as_dictionary()?;
    let number = |key: &str| config.get(key).and_then(plist::Value::as_unsigned_integer);
    Some(RtcpSession {
        local_ssrc: number("RemoteSSRC")? as u32,
        remote_ssrc: number("LocalSSRC")? as u32,
        // RTCPRemotePort (== DestPort) is where the device actually listens.
        // SourcePort is its RTP source and silently discarded our audio reports.
        rtcp_port: number("RTCPRemotePort")
            .or_else(|| number("DestPort"))
            .or_else(|| number("SourcePort"))? as u16,
    })
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

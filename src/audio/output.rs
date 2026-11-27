// language: Rust, file: src/audio/output.rs
//! Audio output backend abstraction and shared observability.

use crate::audio::ring_buffer::PcmRing;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRestartReason {
    Unmute,
    RenderCallbackStalled,
    OutputDeviceChanged,
    MediaSessionRestarted,
    AudioUnitError,
    RepeatedUnderflow,
}

impl std::fmt::Display for AudioRestartReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Unmute => "unmute",
            Self::RenderCallbackStalled => "render-callback-stalled",
            Self::OutputDeviceChanged => "output-device-changed",
            Self::MediaSessionRestarted => "media-session-restarted",
            Self::AudioUnitError => "audiounit-error",
            Self::RepeatedUnderflow => "repeated-underflow",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AudioOutputStats {
    pub backend: &'static str,
    pub callbacks: u64,
    pub silence_frames: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub frames_available: usize,
    pub capacity_frames: usize,
    pub restarts: u64,
    pub last_restart_reason: Option<AudioRestartReason>,
    pub output_device: u32,
}

/// Shared, allocation-free statistics updated from real-time callbacks.
#[derive(Debug, Default)]
pub struct AudioTelemetry {
    pub rtp_packets: AtomicU64,
    pub rtp_loss: AtomicU64,
    pub decode_ok: AtomicU64,
    pub decode_errors: AtomicU64,
    pub frames_written: AtomicU64,
    pub callbacks: AtomicU64,
    pub silence_frames: AtomicU64,
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    pub restarts: AtomicU64,
    pub last_rtp_ms: AtomicU64,
    pub last_pcm_ms: AtomicU64,
    pub last_callback_ms: AtomicU64,
}

impl AudioTelemetry {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Common interface implemented by the AudioUnit and AudioQueue/Linux backends.
pub trait AudioOutput: Send {
    fn backend_name(&self) -> &'static str;

    fn prepare(&mut self) -> Result<()>;

    fn start(&mut self) -> Result<()>;

    fn set_muted(&self, muted: bool);
    fn is_muted(&self) -> bool;

    fn stop(&mut self);

    fn frame_stats(&self) -> AudioOutputStats;

    fn ring(&self) -> &PcmRing;

    fn restart(&mut self, reason: AudioRestartReason) -> Result<()>;

    fn current_output_device(&self) -> Option<u32> {
        None
    }
}

/// Convenience holder for the selected backend.
pub enum SelectedOutput {
    #[cfg(target_os = "macos")]
    AudioUnit(super::audiounit::AudioUnitOutput),
    AudioQueue(super::audioqueue::AudioQueueOutput),
}

impl AudioOutput for SelectedOutput {
    fn backend_name(&self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.backend_name(),
            Self::AudioQueue(o) => o.backend_name(),
        }
    }
    fn prepare(&mut self) -> Result<()> {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.prepare(),
            Self::AudioQueue(o) => o.prepare(),
        }
    }
    fn start(&mut self) -> Result<()> {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.start(),
            Self::AudioQueue(o) => o.start(),
        }
    }
    fn set_muted(&self, muted: bool) {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.set_muted(muted),
            Self::AudioQueue(o) => o.set_muted(muted),
        }
    }
    fn is_muted(&self) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.is_muted(),
            Self::AudioQueue(o) => o.is_muted(),
        }
    }
    fn stop(&mut self) {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.stop(),
            Self::AudioQueue(o) => o.stop(),
        }
    }
    fn frame_stats(&self) -> AudioOutputStats {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.frame_stats(),
            Self::AudioQueue(o) => o.frame_stats(),
        }
    }
    fn ring(&self) -> &PcmRing {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.ring(),
            Self::AudioQueue(o) => o.ring(),
        }
    }
    fn restart(&mut self, reason: AudioRestartReason) -> Result<()> {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.restart(reason),
            Self::AudioQueue(o) => o.restart(reason),
        }
    }
    fn current_output_device(&self) -> Option<u32> {
        match self {
            #[cfg(target_os = "macos")]
            Self::AudioUnit(o) => o.current_output_device(),
            Self::AudioQueue(o) => o.current_output_device(),
        }
    }
}

/// Build the selected output backend with a shared ring buffer and mute state.
pub fn create_output(
    capacity_frames: usize,
) -> (
    SelectedOutput,
    std::sync::Arc<PcmRing>,
    std::sync::Arc<MuteState>,
) {
    use std::sync::Arc;
    let ring = Arc::new(PcmRing::new(capacity_frames));
    let mute = Arc::new(MuteState::default());

    #[cfg(target_os = "macos")]
    let backend = match selected_backend_name() {
        "audioqueue" => SelectedOutput::AudioQueue(super::audioqueue::AudioQueueOutput::new(
            ring.clone(),
            mute.clone(),
        )),
        _ => match super::audiounit::AudioUnitOutput::new(ring.clone(), mute.clone()) {
            Ok(backend) => SelectedOutput::AudioUnit(backend),
            Err(error) => {
                tracing::warn!(%error, "AudioUnit backend unavailable; falling back to AudioQueue");
                SelectedOutput::AudioQueue(super::audioqueue::AudioQueueOutput::new(
                    ring.clone(),
                    mute.clone(),
                ))
            }
        },
    };

    #[cfg(not(target_os = "macos"))]
    let backend = SelectedOutput::AudioQueue(super::audioqueue::AudioQueueOutput::new(
        ring.clone(),
        mute.clone(),
    ));

    (backend, ring, mute)
}

/// Parse `IPHONE_MIRROR_AUDIO_BACKEND`. Defaults to `audiounit` on macOS.
pub fn selected_backend_name() -> &'static str {
    match std::env::var("IPHONE_MIRROR_AUDIO_BACKEND").as_deref() {
        Ok("audioqueue") => "audioqueue",
        _ => "audiounit",
    }
}

pub fn diagnostics_enabled() -> bool {
    matches!(
        std::env::var("IPHONE_MIRROR_AUDIO_DIAGNOSTICS").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Helper used by muted backends to publish callback counters.
#[derive(Debug, Default)]
pub struct MuteState {
    muted: AtomicBool,
    generation: AtomicU64,
}

impl MuteState {
    pub fn set(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }
    pub fn get(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }
    pub fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

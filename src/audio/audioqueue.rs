// language: Rust, file: src/audio/audioqueue.rs
//! AudioQueue fallback backend. A small pump thread pulls PCM from the shared
//! ring buffer and pushes it into AudioQueue every 10 ms. Kept for A/B testing
//! against the AudioUnit render-callback path.

use super::{
    output::{AudioOutput, AudioOutputStats, AudioRestartReason, MuteState},
    ring_buffer::PcmRing,
};
use anyhow::Result;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const FRAMES_PER_PACKET: usize = 480;
const CHANNELS: usize = 2;
const SAMPLES_PER_PACKET: usize = FRAMES_PER_PACKET * CHANNELS;
const PCM_BYTES: usize = SAMPLES_PER_PACKET * 2;

pub struct AudioQueueOutput {
    ring: Arc<PcmRing>,
    mute: Arc<super::output::MuteState>,
    stop: Arc<AtomicBool>,
    returned: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    restarts: u64,
    last_reason: Option<AudioRestartReason>,
    player: Option<super::AudioPlayer>,
}

impl AudioQueueOutput {
    pub fn new(ring: Arc<PcmRing>, mute: Arc<MuteState>) -> Self {
        Self {
            ring,
            mute,
            stop: Arc::new(AtomicBool::new(false)),
            returned: Arc::new(AtomicU64::new(0)),
            thread: None,
            restarts: 0,
            last_reason: None,
            player: None,
        }
    }

    fn spawn_pump(&mut self) -> Result<()> {
        let mut player = self
            .player
            .take()
            .ok_or_else(|| anyhow::anyhow!("AudioQueue player not prepared"))?;
        let ring = self.ring.clone();
        let mute = self.mute.clone();
        let stop = self.stop.clone();
        let handle = thread::Builder::new()
            .name("audio-queue-pump".to_string())
            .spawn(move || {
                let mut samples = vec![0_i16; SAMPLES_PER_PACKET];
                let mut bytes = vec![0_u8; PCM_BYTES];
                while !stop.load(Ordering::Acquire) {
                    let copied = ring.read(&mut samples);
                    let muted = mute.get();
                    let source = if muted {
                        for s in samples.iter_mut() {
                            *s = 0;
                        }
                        &samples[..]
                    } else {
                        &samples[..copied]
                    };
                    for (i, sample) in source.iter().enumerate() {
                        let b = sample.to_le_bytes();
                        bytes[i * 2] = b[0];
                        bytes[i * 2 + 1] = b[1];
                    }
                    if source.len() < SAMPLES_PER_PACKET {
                        let offset = source.len() * 2;
                        for b in bytes[offset..].iter_mut() {
                            *b = 0;
                        }
                    }
                    if let Err(error) = player.play(&bytes) {
                        tracing::debug!(%error, "AudioQueue pump failed to enqueue");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                drop(player);
            })?;
        self.thread = Some(handle);
        Ok(())
    }
}

impl AudioOutput for AudioQueueOutput {
    fn backend_name(&self) -> &'static str {
        "audioqueue"
    }

    fn prepare(&mut self) -> Result<()> {
        if self.player.is_none() {
            self.player = Some(super::AudioPlayer::new(self.returned.clone())?);
        }
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        if self.thread.is_none() {
            self.stop.store(false, Ordering::Release);
            self.spawn_pump()?;
        }
        Ok(())
    }

    fn set_muted(&self, muted: bool) {
        self.mute.set(muted);
    }

    fn is_muted(&self) -> bool {
        self.mute.get()
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        self.player = None;
    }

    fn frame_stats(&self) -> AudioOutputStats {
        let ring = self.ring.stats();
        AudioOutputStats {
            backend: self.backend_name(),
            callbacks: self.returned.load(Ordering::Relaxed),
            silence_frames: ring.silence_frames,
            underruns: ring.underruns,
            overruns: ring.overruns,
            frames_available: ring.frames_available,
            capacity_frames: ring.capacity_frames,
            restarts: self.restarts,
            last_restart_reason: self.last_reason,
            output_device: 0,
        }
    }

    fn ring(&self) -> &PcmRing {
        &self.ring
    }

    fn restart(&mut self, reason: AudioRestartReason) -> Result<()> {
        self.stop();
        self.prepare()?;
        self.start()?;
        self.restarts += 1;
        self.last_reason = Some(reason);
        Ok(())
    }
}

impl Drop for AudioQueueOutput {
    fn drop(&mut self) {
        self.stop();
    }
}

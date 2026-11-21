// language: Rust, file: src/audio/ring_buffer.rs
//! Lock-free single-producer/single-consumer ring buffer for interleaved
//! stereo i16 PCM. The producer is the AAC decode task; the consumer is the
//! real-time AudioUnit render callback.
//!
//! No allocation, locking, or logging happens on the consume path.

use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU64, Ordering},
};

/// One stereo frame is two interleaved i16 samples.
pub const CHANNELS: usize = 2;

pub struct PcmRing {
    /// Fixed storage, allocated once before playback starts.
    samples: Box<[UnsafeCell<i16>]>,
    capacity: u64,
    /// Monotonic sample counters. Avoids ambiguity between empty and full.
    write_pos: AtomicU64,
    read_pos: AtomicU64,
    /// Diagnostics. Only atomics are touched by the real-time callback.
    underruns: AtomicU64,
    overruns: AtomicU64,
    frames_read: AtomicU64,
    silence_frames: AtomicU64,
}

// The ring is deliberately shared between the decode task and the audio
// callback. Access is coordinated by the acquire/release cursor operations.
unsafe impl Send for PcmRing {}
unsafe impl Sync for PcmRing {}

impl PcmRing {
    /// `capacity_frames` stereo frames (each 10 ms at 48 kHz is 480 frames).
    pub fn new(capacity_frames: usize) -> Self {
        let capacity_samples = capacity_frames * CHANNELS;
        let mut samples = Vec::with_capacity(capacity_samples);
        for _ in 0..capacity_samples {
            samples.push(UnsafeCell::new(0_i16));
        }
        Self {
            samples: samples.into_boxed_slice(),
            capacity: capacity_samples as u64,
            write_pos: AtomicU64::new(0),
            read_pos: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            frames_read: AtomicU64::new(0),
            silence_frames: AtomicU64::new(0),
        }
    }

    /// Total capacity in stereo frames.
    pub fn capacity_frames(&self) -> usize {
        (self.capacity / CHANNELS as u64) as usize
    }

    /// Currently buffered stereo frames.
    pub fn frames_available(&self) -> usize {
        let read = self.read_pos.load(Ordering::Acquire);
        let write = self.write_pos.load(Ordering::Relaxed);
        (write.saturating_sub(read) / CHANNELS as u64) as usize
    }

    /// Write interleaved stereo samples. If the ring is full, drop the oldest
    /// frames so the newest audio is always kept and latency cannot grow.
    /// Returns the number of samples actually written.
    pub fn write(&self, pcm: &[i16]) -> usize {
        let len = pcm.len() - (pcm.len() % CHANNELS);
        if len == 0 {
            return 0;
        }
        let mut write = self.write_pos.load(Ordering::Relaxed);
        let read = self.read_pos.load(Ordering::Acquire);
        let occupied = write.saturating_sub(read);
        let incoming = len as u64;

        // Evict oldest frames to make room for the incoming data.
        if occupied + incoming > self.capacity {
            let evict = occupied + incoming - self.capacity;
            let new_read = read + evict;
            self.read_pos.store(new_read, Ordering::Release);
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }

        // Re-read after potential eviction; producer is the only writer of
        // write_pos, so its value cannot change concurrently.
        let base = write;
        for (i, sample) in pcm[..len].iter().enumerate() {
            let index = ((base + i as u64) % self.capacity) as usize;
            // SAFETY: producer owns the region between read_pos and write_pos.
            unsafe { *self.samples[index].get() = *sample };
        }
        write += len as u64;
        self.write_pos.store(write, Ordering::Release);
        len
    }

    /// Fill `out` with interleaved stereo samples. Returns the number of
    /// samples actually copied; the remainder is zero-filled as silence.
    pub fn read(&self, out: &mut [i16]) -> usize {
        let out_len = out.len() - (out.len() % CHANNELS);
        if out_len == 0 {
            return 0;
        }
        let read = self.read_pos.load(Ordering::Relaxed);
        let write = self.write_pos.load(Ordering::Acquire);
        let available = write.saturating_sub(read) as usize;
        let copy = available.min(out_len);

        for (i, slot) in out[..copy].iter_mut().enumerate() {
            let index = ((read + i as u64) % self.capacity) as usize;
            // SAFETY: consumer owns the region between read_pos and write_pos.
            *slot = unsafe { *self.samples[index].get() };
        }
        for slot in out[copy..out_len].iter_mut() {
            *slot = 0;
        }

        self.read_pos
            .store(read + copy as u64, Ordering::Release);
        self.frames_read
            .fetch_add((copy / CHANNELS) as u64, Ordering::Relaxed);
        if copy < out_len {
            self.underruns.fetch_add(1, Ordering::Relaxed);
            self.silence_frames
                .fetch_add(((out_len - copy) / CHANNELS) as u64, Ordering::Relaxed);
        }
        copy
    }

    /// Discard everything currently buffered. Useful on reconnect/unmute.
    pub fn clear(&self) {
        let write = self.write_pos.load(Ordering::Acquire);
        self.read_pos.store(write, Ordering::Release);
    }

    pub fn stats(&self) -> RingStats {
        RingStats {
            frames_available: self.frames_available(),
            capacity_frames: self.capacity_frames(),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            frames_read: self.frames_read.load(Ordering::Relaxed),
            silence_frames: self.silence_frames.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RingStats {
    pub frames_available: usize,
    pub capacity_frames: usize,
    pub underruns: u64,
    pub overruns: u64,
    pub frames_read: u64,
    pub silence_frames: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read() {
        let ring = PcmRing::new(16);
        let pcm: Vec<i16> = (0..8).collect();
        assert_eq!(ring.write(&pcm), 8);
        let mut out = vec![0_i16; 8];
        assert_eq!(ring.read(&mut out), 8);
        assert_eq!(out, pcm);
    }

    #[test]
    fn read_silence_when_empty() {
        let ring = PcmRing::new(16);
        let mut out = vec![7_i16; 8];
        assert_eq!(ring.read(&mut out), 0);
        assert_eq!(out, vec![0_i16; 8]);
        assert_eq!(ring.stats().underruns, 1);
        assert_eq!(ring.stats().silence_frames, 4);
    }

    #[test]
    fn overrun_drops_oldest() {
        let ring = PcmRing::new(4); // 4 frames = 8 samples
        ring.write(&[1, 1, 2, 2]); // frames 1,2
        ring.write(&[3, 3, 4, 4]); // frames 3,4 -> full
        ring.write(&[5, 5, 6, 6]); // evicts frames 1,2
        let mut out = vec![0_i16; 8];
        assert_eq!(ring.read(&mut out), 8);
        assert_eq!(out, vec![3, 3, 4, 4, 5, 5, 6, 6]);
        assert_eq!(ring.stats().overruns, 1);
    }

    #[test]
    fn wraparound() {
        let ring = PcmRing::new(4);
        for round in 0..10 {
            let a = round * 4 + 1;
            let b = a + 1;
            ring.write(&[a, a, b, b]);
            let mut out = vec![0_i16; 4];
            assert_eq!(ring.read(&mut out), 4);
            assert_eq!(out, vec![a, a, b, b]);
        }
    }

    #[test]
    fn partial_read() {
        let ring = PcmRing::new(16);
        ring.write(&[1, 1, 2, 2, 3, 3]);
        let mut out = vec![0_i16; 12];
        assert_eq!(ring.read(&mut out), 6);
        assert_eq!(&out[..6], &[1, 1, 2, 2, 3, 3]);
        assert_eq!(&out[6..], &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn clear_discards_buffer() {
        let ring = PcmRing::new(8);
        ring.write(&[1, 1, 2, 2]);
        ring.clear();
        assert_eq!(ring.frames_available(), 0);
    }
}

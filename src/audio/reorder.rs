// language: Rust, file: src/audio/reorder.rs
//! Small RTP reorder buffer for the audio stream. Holds late packets briefly,
//! releases them in sequence order, and reports gaps as silence frames so a
//! missing access unit never corrupts the AAC decoder stream.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub sequence: u32,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum ReorderedAudio {
    Packet(AudioPacket),
    Silence,
}

pub struct AudioReorder {
    pending: BTreeMap<u32, AudioPacket>,
    next: Option<u32>,
    window: u32,
    timeout: Duration,
    first_pending: Option<Instant>,
}

impl AudioReorder {
    pub fn new(window: u32, timeout: Duration) -> Self {
        Self {
            pending: BTreeMap::new(),
            next: None,
            window,
            timeout,
            first_pending: None,
        }
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.next = None;
        self.first_pending = None;
    }

    /// Insert a packet keyed by its extended sequence number.
    pub fn push(&mut self, packet: AudioPacket) {
        let seq = packet.sequence;
        if self
            .next
            .is_some_and(|next| seq.wrapping_add(self.window) < next || seq < next.wrapping_sub(self.window))
        {
            // Too far out of window: discard stale packet.
            return;
        }
        if !self.pending.contains_key(&seq) {
            self.pending.insert(seq, packet);
            if self.first_pending.is_none() {
                self.first_pending = Some(Instant::now());
            }
        }
    }

    /// Release packets that are ready in sequence order.
    pub fn pop_ready(&mut self) -> Vec<ReorderedAudio> {
        let mut out = Vec::new();
        let mut next = match self.next {
            Some(n) => n,
            None => match self.pending.keys().copied().min() {
                Some(n) => n,
                None => return out,
            },
        };

        // Drain a contiguous run from the expected sequence.
        while let Some(packet) = self.pending.remove(&next) {
            out.push(ReorderedAudio::Packet(packet));
            next = next.wrapping_add(1);
        }

        // If a gap remains and the oldest buffered packet has waited long
        // enough, emit silence for the missing sequence and continue draining.
        loop {
            let Some(&oldest) = self.pending.keys().next() else { break };
            if oldest == next {
                break;
            }
            let timed_out = self
                .first_pending
                .is_some_and(|t| t.elapsed() >= self.timeout);
            if !timed_out {
                break;
            }
            out.push(ReorderedAudio::Silence);
            next = next.wrapping_add(1);
            while let Some(packet) = self.pending.remove(&next) {
                out.push(ReorderedAudio::Packet(packet));
                next = next.wrapping_add(1);
            }
        }

        self.next = Some(next);
        if self.pending.is_empty() {
            self.first_pending = None;
        }
        out
    }

    pub fn stats(&self) -> ReorderStats {
        ReorderStats {
            buffered: self.pending.len(),
            next: self.next,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReorderStats {
    pub buffered: usize,
    pub next: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32) -> AudioPacket {
        AudioPacket { sequence: seq, timestamp: seq * 960, payload: vec![seq as u8] }
    }

    #[test]
    fn in_order_passthrough() {
        let mut r = AudioReorder::new(8, Duration::from_millis(20));
        r.push(pkt(10));
        let out = r.pop_ready();
        assert_eq!(out.len(), 1);
        match &out[0] { ReorderedAudio::Packet(p) => assert_eq!(p.sequence, 10), _ => panic!() }
    }

    #[test]
    fn reorders_late_packet() {
        let mut r = AudioReorder::new(8, Duration::from_millis(20));
        r.push(pkt(11));
        r.push(pkt(10)); // 10 arrives after 11 but before timeout
        let out = r.pop_ready();
        assert_eq!(out.len(), 2);
        match &out[0] { ReorderedAudio::Packet(p) => assert_eq!(p.sequence, 10), _ => panic!() }
        match &out[1] { ReorderedAudio::Packet(p) => assert_eq!(p.sequence, 11), _ => panic!() }
    }

    #[test]
    fn gap_emits_silence_after_timeout() {
        let mut r = AudioReorder::new(8, Duration::from_millis(5));
        r.push(pkt(10));
        r.pop_ready();
        r.push(pkt(12)); // 11 missing
        std::thread::sleep(Duration::from_millis(10));
        let out = r.pop_ready();
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], ReorderedAudio::Silence));
        match &out[1] { ReorderedAudio::Packet(p) => assert_eq!(p.sequence, 12), _ => panic!() }
    }

    #[test]
    fn duplicate_is_ignored() {
        let mut r = AudioReorder::new(8, Duration::from_millis(20));
        r.push(pkt(10));
        r.push(pkt(10));
        assert_eq!(r.stats().buffered, 1);
    }

    #[test]
    fn reset_clears_state() {
        let mut r = AudioReorder::new(8, Duration::from_millis(20));
        r.push(pkt(10));
        r.reset();
        assert_eq!(r.stats().buffered, 0);
        assert_eq!(r.stats().next, None);
    }
}

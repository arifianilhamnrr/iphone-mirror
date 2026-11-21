// language: Rust, file: src/audio/jitter.rs
//! Adaptive jitter target and RTP statistics for the audio stream.

/// Tracks RTP sequence/arrival statistics and adapts the desired buffer depth.
pub struct AudioJitter {
    pub target_frames: usize,
    min_target: usize,
    max_target: usize,
    base_target: usize,
    stable_ticks: u32,
    last_extended: Option<u32>,
    expected: u64,
    received: u64,
    lost: u64,
    duplicate: u64,
    reordered: u64,
}

impl AudioJitter {
    pub fn new(base_target: usize) -> Self {
        Self {
            target_frames: base_target,
            min_target: base_target.max(4),
            max_target: 20_000,
            base_target,
            stable_ticks: 0,
            last_extended: None,
            expected: 0,
            received: 0,
            lost: 0,
            duplicate: 0,
            reordered: 0,
        }
    }

    /// Update statistics from an RTP sequence number (extended by caller).
    pub fn observe(&mut self, extended: u32) {
        self.received += 1;
        match self.last_extended {
            None => self.last_extended = Some(extended),
            Some(last) => {
                if extended == last {
                    self.duplicate += 1;
                } else if extended < last {
                    // Late/out-of-order arrival.
                    self.reordered += 1;
                } else {
                    let gap = (extended - last) as u64;
                    self.expected += gap;
                    if gap > 1 {
                        self.lost += gap - 1;
                        // Increase target on loss bursts.
                        self.target_frames = (self.target_frames + 2).min(self.max_target);
                        self.stable_ticks = 0;
                    }
                    self.last_extended = Some(extended);
                }
            }
        }
    }

    /// Called when the output ring experienced an underrun.
    pub fn on_underflow(&mut self) {
        self.target_frames = (self.target_frames + 2).min(self.max_target);
        self.stable_ticks = 0;
    }

    /// Called periodically while playback is healthy.
    pub fn tick_stable(&mut self) {
        self.stable_ticks += 1;
        if self.stable_ticks >= 60 {
            // Reduce target slowly back toward base after a stable minute.
            if self.target_frames > self.base_target {
                self.target_frames -= 1;
            }
            self.stable_ticks = 0;
        }
    }

    pub fn stats(&self) -> JitterStats {
        JitterStats {
            target_frames: self.target_frames,
            lost: self.lost,
            duplicate: self.duplicate,
            reordered: self.reordered,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct JitterStats {
    pub target_frames: usize,
    pub lost: u64,
    pub duplicate: u64,
    pub reordered: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observes_loss_and_increases_target() {
        let mut j = AudioJitter::new(100);
        j.observe(10);
        j.observe(15);
        assert_eq!(j.stats().lost, 4);
        assert_eq!(j.stats().target_frames, 102);
    }

    #[test]
    fn underflow_increases_target() {
        let mut j = AudioJitter::new(100);
        j.on_underflow();
        assert_eq!(j.stats().target_frames, 102);
    }

    #[test]
    fn stable_decreases_target() {
        let mut j = AudioJitter::new(100);
        j.observe(1);
        j.observe(3); // loss, target +2
        for _ in 0..60 {
            j.tick_stable();
        }
        assert_eq!(j.stats().target_frames, 101);
    }

    #[test]
    fn detects_duplicate_and_reorder() {
        let mut j = AudioJitter::new(100);
        j.observe(10);
        j.observe(10);
        j.observe(9);
        let s = j.stats();
        assert_eq!(s.duplicate, 1);
        assert_eq!(s.reordered, 1);
    }
}

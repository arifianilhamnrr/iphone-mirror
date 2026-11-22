// language: Rust, file: src/protocol/rtcp_stats.rs
//! RTP receiver statistics for accurate RTCP Receiver Reports.

use std::time::Instant;

#[derive(Debug, Clone)]
pub struct RtcpReportStats {
    pub base_sequence: Option<u32>,
    pub extended_max: u32,
    pub expected: u32,
    pub received: u32,
    pub cumulative_lost: i32,
    pub interval_lost: i32,
    pub fraction_lost: u8,
    pub jitter: u32,
    pub lsr: u32,
    pub dlsr: u32,
    last_timestamp: Option<u32>,
    last_arrival: Option<Instant>,
    last_interval: Instant,
    last_received: u32,
    last_cumulative_lost: i32,
}

impl RtcpReportStats {
    pub fn new() -> Self {
        Self {
            base_sequence: None,
            extended_max: 0,
            expected: 0,
            received: 0,
            cumulative_lost: 0,
            interval_lost: 0,
            fraction_lost: 0,
            jitter: 0,
            lsr: 0,
            dlsr: 0,
            last_timestamp: None,
            last_arrival: None,
            last_interval: Instant::now(),
            last_received: 0,
            last_cumulative_lost: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Observe a received RTP packet.
    pub fn observe(&mut self, extended_seq: u32, rtp_timestamp: u32) {
        if self.base_sequence.is_none() {
            self.base_sequence = Some(extended_seq);
        }
        self.received = self.received.wrapping_add(1);
        if self.extended_max == 0 || extended_seq > self.extended_max {
            self.extended_max = extended_seq;
        }

        // RFC 3550 interarrival jitter, in RTP timestamp units.
        let now = Instant::now();
        if let (Some(last_ts), Some(last_arrival)) = (self.last_timestamp, self.last_arrival) {
            let transit = now.duration_since(last_arrival).as_secs_f64() * 48_000.0;
            let d = (rtp_timestamp as f64 - last_ts as f64) - transit;
            let abs = d.abs() as u32;
            self.jitter += (abs - ((self.jitter + 8) >> 4)) / 16;
        }
        self.last_timestamp = Some(rtp_timestamp);
        self.last_arrival = Some(now);
    }

    /// Recompute interval/fraction loss. Call before building a report.
    pub fn refresh(&mut self) {
        let base = self.base_sequence.unwrap_or(self.extended_max);
        let expected = self.extended_max.wrapping_sub(base).wrapping_add(1);
        self.expected = expected;
        let received = self.received;
        let lost = expected as i64 - received as i64;
        self.cumulative_lost = lost.clamp(i32::MIN as i64, i32::MAX as i64) as i32;

        // Interval counters are deltas since the previous report.
        let received_delta = self.received.wrapping_sub(self.last_received) as i32;
        let lost_delta = self.cumulative_lost.wrapping_sub(self.last_cumulative_lost);
        self.interval_lost = lost_delta.max(0);
        self.last_received = self.received;
        self.last_cumulative_lost = self.cumulative_lost;
        self.last_interval = Instant::now();

        let expected_interval = received_delta + self.interval_lost;
        self.fraction_lost = if expected_interval > 0 {
            (((self.interval_lost as f32 / expected_interval as f32) * 256.0) as u8).min(255)
        } else {
            0
        };
    }

    /// Set last Sender Report middle-32-bit timestamp and current wall clock.
    pub fn observe_sender_report(&mut self, ntp_middle: u32) {
        self.lsr = ntp_middle;
        self.dlsr = 0;
        self.last_arrival = Some(Instant::now());
    }

    /// Update delay since last SR in 1/65536 second units.
    pub fn refresh_dlsr(&mut self) {
        if self.lsr != 0 {
            if let Some(arrival) = self.last_arrival {
                let d = arrival.elapsed().as_secs_f64() * 65536.0;
                self.dlsr = (d as u32).min(u32::MAX);
            }
        }
    }

    pub fn extended_highest(&self) -> u32 {
        self.extended_max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_empty() {
        let s = RtcpReportStats::new();
        assert_eq!(s.extended_highest(), 0);
        assert_eq!(s.fraction_lost, 0);
    }

    #[test]
    fn observes_sequence_and_timestamp() {
        let mut s = RtcpReportStats::new();
        s.observe(10, 9600);
        s.observe(11, 10080);
        assert_eq!(s.extended_highest(), 11);
        assert_eq!(s.received, 2);
    }

    #[test]
    fn reset_clears_state() {
        let mut s = RtcpReportStats::new();
        s.observe(10, 9600);
        s.reset();
        assert_eq!(s.extended_highest(), 0);
    }

    #[test]
    fn sender_report_updates_lsr() {
        let mut s = RtcpReportStats::new();
        s.observe_sender_report(0x1234_5678);
        assert_eq!(s.lsr, 0x1234_5678);
    }
}

// language: Rust, file: src/input/hid_packets.rs
//! Serializer for CoreDevice ScreenStreamServer HID touch events.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down = 1,
    Move = 2,
    Up = 3,
    Cancel = 4,
}

#[derive(Debug, Clone)]
pub struct TouchEvent {
    pub finger_id: u32,
    pub phase: TouchPhase,
    /// Normalized coordinates [0.0 .. 1.0]
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

impl TouchEvent {
    pub fn new(finger_id: u32, phase: TouchPhase, x: f32, y: f32) -> Self {
        Self {
            finger_id,
            phase,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            pressure: 1.0,
        }
    }

    /// Serialize event into binary payload recognized by RemoteXPC displayservice HID channel
    pub fn serialize(&self, timestamp_us: u64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(32);
        // Header marker (0x01 = Touch Packet)
        buf.push(0x01);
        buf.extend_from_slice(&(self.finger_id).to_be_bytes());
        buf.push(self.phase as u8);
        buf.extend_from_slice(&self.x.to_be_bytes());
        buf.extend_from_slice(&self.y.to_be_bytes());
        buf.extend_from_slice(&self.pressure.to_be_bytes());
        buf.extend_from_slice(&timestamp_us.to_be_bytes());
        buf
    }
}

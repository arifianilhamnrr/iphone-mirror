// language: Rust, file: src/input/input_handler.rs
use crate::input::coordinate_mapper::CoordinateMapper;
use crate::input::hid_packets::{TouchEvent, TouchPhase};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

pub struct InputHandler {
    pub mapper: CoordinateMapper,
    is_mouse_down: bool,
}

impl InputHandler {
    pub fn new(window_width: f32, window_height: f32) -> Self {
        Self {
            mapper: CoordinateMapper::new(window_width, window_height),
            is_mouse_down: false,
        }
    }

    pub fn on_mouse_down(&mut self, x: f32, y: f32) -> Option<Vec<u8>> {
        self.is_mouse_down = true;
        let (nx, ny) = self.mapper.map_to_normalized(x, y);
        let event = TouchEvent::new(0, TouchPhase::Down, nx, ny);
        debug!(nx, ny, "Touch Down");
        Some(event.serialize(Self::timestamp_us()))
    }

    pub fn on_mouse_move(&mut self, x: f32, y: f32) -> Option<Vec<u8>> {
        if !self.is_mouse_down {
            return None;
        }
        let (nx, ny) = self.mapper.map_to_normalized(x, y);
        let event = TouchEvent::new(0, TouchPhase::Move, nx, ny);
        Some(event.serialize(Self::timestamp_us()))
    }

    pub fn on_mouse_up(&mut self, x: f32, y: f32) -> Option<Vec<u8>> {
        if !self.is_mouse_down {
            return None;
        }
        self.is_mouse_down = false;
        let (nx, ny) = self.mapper.map_to_normalized(x, y);
        let event = TouchEvent::new(0, TouchPhase::Up, nx, ny);
        debug!(nx, ny, "Touch Up");
        Some(event.serialize(Self::timestamp_us()))
    }

    fn timestamp_us() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }
}

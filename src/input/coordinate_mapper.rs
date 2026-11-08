// language: Rust, file: src/input/coordinate_mapper.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Portrait = 0,
    Landscape270 = 270,
}

pub struct CoordinateMapper {
    pub window_width: f32,
    pub window_height: f32,
    pub orientation: Orientation,
}

impl CoordinateMapper {
    pub fn new(window_width: f32, window_height: f32) -> Self {
        Self {
            window_width,
            window_height,
            orientation: Orientation::Portrait,
        }
    }

    /// Map pixel coordinates within window to normalized [0.0 .. 1.0] iPhone touch space
    pub fn map_to_normalized(&self, x: f32, y: f32) -> (f32, f32) {
        if self.window_width <= 0.0 || self.window_height <= 0.0 {
            return (0.0, 0.0);
        }

        let norm_x = (x / self.window_width).clamp(0.0, 1.0);
        let norm_y = (y / self.window_height).clamp(0.0, 1.0);

        match self.orientation {
            Orientation::Portrait => (norm_x, norm_y),
            Orientation::Landscape270 => {
                // 270 degree rotation transformation matrix
                let rot_x = norm_y;
                let rot_y = 1.0 - norm_x;
                (rot_x, rot_y)
            }
        }
    }

    /// Map a point from a letterboxed window into normalized video coordinates.
    /// Points in the surrounding bars are intentionally rejected.
    pub fn map_aspect_fit(
        window_width: f64,
        window_height: f64,
        content_width: f64,
        content_height: f64,
        x: f64,
        y: f64,
    ) -> Option<(f32, f32)> {
        if window_width <= 0.0
            || window_height <= 0.0
            || content_width <= 0.0
            || content_height <= 0.0
        {
            return None;
        }

        let scale = (window_width / content_width).min(window_height / content_height);
        let fitted_width = content_width * scale;
        let fitted_height = content_height * scale;
        let offset_x = (window_width - fitted_width) * 0.5;
        let offset_y = (window_height - fitted_height) * 0.5;

        if x < offset_x
            || y < offset_y
            || x > offset_x + fitted_width
            || y > offset_y + fitted_height
        {
            return None;
        }

        Some((
            ((x - offset_x) / fitted_width).clamp(0.0, 1.0) as f32,
            ((y - offset_y) / fitted_height).clamp(0.0, 1.0) as f32,
        ))
    }
}

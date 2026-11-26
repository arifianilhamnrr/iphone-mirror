// language: Rust, file: src/input/coordinate_mapper.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Portrait = 0,
    Landscape270 = 270,
}

/// Display orientation relative to the device's native portrait framebuffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DisplayRotation {
    #[default]
    Portrait = 0,
    /// Device rotated 90° clockwise (home button / bottom bar on the left of the
    /// portrait buffer, top bar on the right). Needs 90° CCW display rotation.
    LandscapeRight = 1,
    /// Device rotated 90° counter-clockwise. Needs 90° CW display rotation.
    LandscapeLeft = 2,
    PortraitUpsideDown = 3,
}

impl DisplayRotation {
    pub fn is_landscape(self) -> bool {
        matches!(self, Self::LandscapeRight | Self::LandscapeLeft)
    }

    pub fn shader_mode(self) -> u32 {
        self as u32
    }

    pub fn from_shader_mode(mode: u32) -> Self {
        match mode {
            1 => Self::LandscapeRight,
            2 => Self::LandscapeLeft,
            3 => Self::PortraitUpsideDown,
            _ => Self::Portrait,
        }
    }

    pub fn cycle_manual(self) -> Self {
        match self {
            Self::Portrait => Self::LandscapeRight,
            Self::LandscapeRight => Self::LandscapeLeft,
            Self::LandscapeLeft | Self::PortraitUpsideDown => Self::Portrait,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::LandscapeRight => "landscape-right",
            Self::LandscapeLeft => "landscape-left",
            Self::PortraitUpsideDown => "portrait-upside-down",
        }
    }

    /// Map normalized displayed coordinates `(u, v)` into native portrait
    /// framebuffer coordinates `(bx, by)`.
    pub fn to_buffer_coords(self, u: f32, v: f32) -> (f32, f32) {
        match self {
            Self::Portrait => (u, v),
            Self::LandscapeRight => (1.0 - v, u),
            Self::LandscapeLeft => (v, 1.0 - u),
            Self::PortraitUpsideDown => (1.0 - u, 1.0 - v),
        }
    }

    /// Map `0..=u16::MAX` displayed touchscreen coordinates into native portrait
    /// framebuffer coordinates.
    pub fn to_buffer_u16(self, x: u16, y: u16) -> (u16, u16) {
        match self {
            Self::Portrait => (x, y),
            Self::LandscapeRight => (u16::MAX - y, x),
            Self::LandscapeLeft => (y, u16::MAX - x),
            Self::PortraitUpsideDown => (u16::MAX - x, u16::MAX - y),
        }
    }

    /// Transform a displayed drag delta `(dx, dy)` into native portrait
    /// framebuffer delta `(dbx, dby)`.
    pub fn rotate_delta(self, dx: i32, dy: i32) -> (i32, i32) {
        match self {
            Self::Portrait => (dx, dy),
            Self::LandscapeRight => (-dy, dx),
            Self::LandscapeLeft => (dy, -dx),
            Self::PortraitUpsideDown => (-dx, -dy),
        }
    }

    pub fn rotate_delta_f64(self, dx: f64, dy: f64) -> (f64, f64) {
        match self {
            Self::Portrait => (dx, dy),
            Self::LandscapeRight => (-dy, dx),
            Self::LandscapeLeft => (dy, -dx),
            Self::PortraitUpsideDown => (-dx, -dy),
        }
    }
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

    /// Map a point from a letterboxed window into native portrait framebuffer
    /// coordinates, accounting for `rotation`.
    pub fn map_aspect_fit_rotated(
        window_width: f64,
        window_height: f64,
        frame_width: f64,
        frame_height: f64,
        rotation: DisplayRotation,
        x: f64,
        y: f64,
    ) -> Option<(f32, f32)> {
        let (content_width, content_height) = if rotation.is_landscape() && frame_width < frame_height {
            (frame_height, frame_width)
        } else {
            (frame_width, frame_height)
        };
        let (u, v) = Self::map_aspect_fit(
            window_width,
            window_height,
            content_width,
            content_height,
            x,
            y,
        )?;
        if frame_width > frame_height && rotation.is_landscape() {
            Some((u, v))
        } else {
            Some(rotation.to_buffer_coords(u, v))
        }
    }
}

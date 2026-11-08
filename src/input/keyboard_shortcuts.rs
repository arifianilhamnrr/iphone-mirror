// language: Rust, file: src/input/keyboard_shortcuts.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemShortcut {
    HomeSwipe,
    AppSwitcher,
    LockScreen,
    VolumeUp,
    VolumeDown,
    ToggleKeyboard,
    RotateLandscape,
}

pub struct KeyboardShortcutHandler;

impl KeyboardShortcutHandler {
    /// Evaluates if a key combination matches a mirror command
    /// cmd_held: Command key state (macOS Super key)
    pub fn match_shortcut(key_char: char, cmd_held: bool) -> Option<SystemShortcut> {
        if !cmd_held {
            return None;
        }

        match key_char.to_ascii_lowercase() {
            'h' => Some(SystemShortcut::HomeSwipe),
            's' => Some(SystemShortcut::AppSwitcher),
            'l' => Some(SystemShortcut::LockScreen),
            'k' => Some(SystemShortcut::ToggleKeyboard),
            'r' => Some(SystemShortcut::RotateLandscape),
            _ => None,
        }
    }

    /// Serialize hardware button event for CoreDevice
    pub fn serialize_button_event(shortcut: SystemShortcut) -> Vec<u8> {
        let (usage_page, usage_id) = match shortcut {
            SystemShortcut::LockScreen => (0x0C, 0x0030), // Consumer Power
            SystemShortcut::VolumeUp => (0x0C, 0x00E9),   // Volume Increment
            SystemShortcut::VolumeDown => (0x0C, 0x00EA), // Volume Decrement
            SystemShortcut::ToggleKeyboard => (0x0C, 0x00B8), // Eject (toggles virtual keyboard)
            _ => (0x0C, 0x0000),
        };

        let mut buf = Vec::with_capacity(8);
        buf.push(0x02); // 0x02 = Button packet
        buf.extend_from_slice(&(usage_page as u16).to_be_bytes());
        buf.extend_from_slice(&(usage_id as u16).to_be_bytes());
        buf
    }
}

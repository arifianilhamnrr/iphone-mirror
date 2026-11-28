use crate::notifications::ancs::ANCS_SERVICE_UUID;
use anyhow::Result;
use std::process::Command;

/// Snapshot of BlueZ adapter & ANCS availability on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluezAncsStatus {
    pub adapter_powered: bool,
    pub bonded_iphones: Vec<String>,
    pub detail: String,
}

/// Checks BlueZ Bluetooth adapter readiness via `bluetoothctl show`.
pub fn inspect_bluez_status() -> BluezAncsStatus {
    let Ok(output) = Command::new("bluetoothctl").arg("show").output() else {
        return BluezAncsStatus {
            adapter_powered: false,
            bonded_iphones: Vec::new(),
            detail: "bluetoothctl unavailable (install bluez for BLE ANCS)".into(),
        };
    };
    if !output.status.success() {
        return BluezAncsStatus {
            adapter_powered: false,
            bonded_iphones: Vec::new(),
            detail: "No Bluetooth adapter active".into(),
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let powered = parse_powered_from_bluetoothctl(&text);
    let bonded = list_bonded_devices();
    let detail = if !powered {
        "Bluetooth adapter is turned off".into()
    } else if bonded.is_empty() {
        "Bluetooth powered on; pair your iPhone and enable Share System Notifications".into()
    } else {
        format!("Bluetooth ready ({})", bonded.join(", "))
    };
    BluezAncsStatus {
        adapter_powered: powered,
        bonded_iphones: bonded,
        detail,
    }
}

/// Best-effort configuration of BlueZ ANCS service solicitation advertisement via `bluetoothctl`.
pub fn solicit_ancs_advertisement() -> Result<()> {
    let script = format!(
        "power on\nadvertise.solicit {}\nadvertise on\n",
        ANCS_SERVICE_UUID.to_ascii_lowercase()
    );
    let mut child = Command::new("bluetoothctl")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(script.as_bytes());
    }
    let _ = child.wait();
    Ok(())
}

fn parse_powered_from_bluetoothctl(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.eq_ignore_ascii_case("Powered: yes")
    })
}

fn list_bonded_devices() -> Vec<String> {
    let Ok(out) = Command::new("bluetoothctl")
        .args(["devices", "Paired"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // Format: `Device AA:BB:CC:DD:EE:FF Name`
            let mut parts = line.splitn(3, ' ');
            let kind = parts.next()?;
            let _mac = parts.next()?;
            let name = parts.next()?.trim();
            if kind == "Device" && !name.is_empty() {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bluetoothctl_powered_output() {
        let sample = "Controller 00:1A:7D:DA:71:13 (public)\n\tName: archlinux\n\tPowered: yes\n";
        assert!(parse_powered_from_bluetoothctl(sample));
        assert!(!parse_powered_from_bluetoothctl("\tPowered: no\n"));
    }
}

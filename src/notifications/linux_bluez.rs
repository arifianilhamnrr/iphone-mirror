use crate::notifications::ancs::{
    ANCS_CONTROL_POINT_UUID, ANCS_DATA_SOURCE_UUID, ANCS_NOTIFICATION_SOURCE_UUID,
    ANCS_SERVICE_UUID,
};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::export::futures_util::StreamExt;
use zbus::fdo::{ManagedObjects, ObjectManagerProxy, PropertiesProxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection};

const ADV_OBJECT_PATH: &str = "/id/web/projectar/iphone_mirror/adv0";

/// Snapshot of BlueZ adapter & ANCS availability on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluezAncsStatus {
    pub adapter_powered: bool,
    pub bonded_iphones: Vec<String>,
    pub detail: String,
}

/// Real-time events emitted by the Linux BlueZ D-Bus ANCS GATT Client.
#[derive(Debug)]
pub enum BluezAncsEvent {
    Status(String),
    Connected { device_name: String },
    Disconnected { reason: String },
    CommandChannelReady(mpsc::UnboundedSender<Vec<u8>>),
    NotificationSourcePacket(Vec<u8>),
    DataSourcePacket(Vec<u8>),
}

/// BlueZ LE Advertisement advertising ANCS Service Solicitation (`7905F431-...`).
struct AncsAdvertisement;

#[interface(name = "org.bluez.LEAdvertisement1")]
impl AncsAdvertisement {
    #[zbus(property, name = "Type")]
    fn ad_type(&self) -> &str {
        "peripheral"
    }

    #[zbus(property, name = "LocalName")]
    fn local_name(&self) -> &str {
        "iPhone Mirror"
    }

    #[zbus(property, name = "SolicitUUIDs")]
    fn solicit_uuids(&self) -> Vec<String> {
        vec![ANCS_SERVICE_UUID.to_ascii_lowercase()]
    }

    fn release(&self) {}
}

#[derive(Debug, Clone)]
struct AncsDeviceEndpoints {
    device_name: String,
    device_path: String,
    notif_source_path: String,
    data_source_path: String,
    control_point_path: String,
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

/// Spawns the full async BlueZ D-Bus ANCS GATT client + BLE Service Solicitation loop.
pub fn spawn_bluez_ancs_loop(event_tx: mpsc::UnboundedSender<BluezAncsEvent>) {
    tokio::spawn(async move {
        if let Err(err) = run_bluez_ancs_loop(event_tx.clone()).await {
            let _ = event_tx.send(BluezAncsEvent::Status(format!(
                "BlueZ ANCS D-Bus inactive: {err}"
            )));
        }
    });
}

async fn run_bluez_ancs_loop(event_tx: mpsc::UnboundedSender<BluezAncsEvent>) -> Result<()> {
    let conn = Connection::system()
        .await
        .context("Failed to connect to system D-Bus")?;
    let om = ObjectManagerProxy::builder(&conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;

    // Expose org.bluez.LEAdvertisement1 object and register ANCS solicitation on first adapter.
    let _ = conn
        .object_server()
        .at(ADV_OBJECT_PATH, AncsAdvertisement)
        .await;
    let initial_objects = om
        .get_managed_objects()
        .await
        .context("BlueZ org.bluez service unavailable")?;
    if let Some(adapter_path) = find_first_adapter(&initial_objects) {
        let adv_path = ObjectPath::try_from(ADV_OBJECT_PATH)?;
        let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
        match conn
            .call_method(
                Some("org.bluez"),
                adapter_path.as_str(),
                Some("org.bluez.LEAdvertisingManager1"),
                "RegisterAdvertisement",
                &(adv_path, empty_opts),
            )
            .await
        {
            Ok(_) => {
                let _ = event_tx.send(BluezAncsEvent::Status(
                    "BLE ANCS soliciting — waiting for paired iPhone connection…".into(),
                ));
            }
            Err(err) => {
                let _ = event_tx.send(BluezAncsEvent::Status(format!(
                    "BlueZ listening for paired iPhone (LE Adv note: {err})"
                )));
            }
        }
    }

    loop {
        let objects = match om.get_managed_objects().await {
            Ok(objs) => objs,
            Err(err) => {
                let _ = event_tx.send(BluezAncsEvent::Status(format!(
                    "Waiting for BlueZ service: {err}"
                )));
                tokio::time::sleep(Duration::from_secs(4)).await;
                continue;
            }
        };

        if let Some(ep) = find_ancs_endpoints(&objects) {
            if let Err(err) = run_connected_ancs_session(&conn, &ep, &event_tx).await {
                let _ = event_tx.send(BluezAncsEvent::Disconnected {
                    reason: err.to_string(),
                });
            }
            let _ = event_tx.send(BluezAncsEvent::Status(
                "iPhone ANCS disconnected — waiting for reconnect…".into(),
            ));
        } else {
            let status_hint = summarize_pairing_hint(&objects);
            let _ = event_tx.send(BluezAncsEvent::Status(status_hint));
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_connected_ancs_session(
    conn: &Connection,
    ep: &AncsDeviceEndpoints,
    event_tx: &mpsc::UnboundedSender<BluezAncsEvent>,
) -> Result<()> {
    let data_props = PropertiesProxy::builder(conn)
        .destination("org.bluez")?
        .path(ep.data_source_path.as_str())?
        .build()
        .await?;
    let notif_props = PropertiesProxy::builder(conn)
        .destination("org.bluez")?
        .path(ep.notif_source_path.as_str())?
        .build()
        .await?;
    let dev_props = PropertiesProxy::builder(conn)
        .destination("org.bluez")?
        .path(ep.device_path.as_str())?
        .build()
        .await?;

    let mut data_stream = data_props.receive_properties_changed().await?;
    let mut notif_stream = notif_props.receive_properties_changed().await?;
    let mut dev_stream = dev_props.receive_properties_changed().await?;

    // Apple ANCS spec: subscribe Data Source first, then Notification Source.
    conn.call_method(
        Some("org.bluez"),
        ep.data_source_path.as_str(),
        Some("org.bluez.GattCharacteristic1"),
        "StartNotify",
        &(),
    )
    .await
    .context(
        "Failed to StartNotify on ANCS Data Source (check iPhone Share System Notifications)",
    )?;

    tokio::time::sleep(Duration::from_millis(120)).await;

    if let Err(err) = conn
        .call_method(
            Some("org.bluez"),
            ep.notif_source_path.as_str(),
            Some("org.bluez.GattCharacteristic1"),
            "StartNotify",
            &(),
        )
        .await
    {
        let _ = conn
            .call_method(
                Some("org.bluez"),
                ep.data_source_path.as_str(),
                Some("org.bluez.GattCharacteristic1"),
                "StopNotify",
                &(),
            )
            .await;
        return Err(err).context("Failed to StartNotify on ANCS Notification Source");
    }

    let (cp_tx, mut cp_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let _ = event_tx.send(BluezAncsEvent::CommandChannelReady(cp_tx));
    let _ = event_tx.send(BluezAncsEvent::Connected {
        device_name: ep.device_name.clone(),
    });
    let _ = event_tx.send(BluezAncsEvent::Status(format!(
        "ANCS active: {}",
        ep.device_name
    )));

    loop {
        tokio::select! {
            sig = data_stream.next() => {
                let Some(sig) = sig else { break; };
                if let Ok(args) = sig.args() {
                    if let Some(val) = args.changed_properties.get("Value") {
                        if let Some(bytes) = extract_bytes_from_value(val) {
                            let _ = event_tx.send(BluezAncsEvent::DataSourcePacket(bytes));
                        }
                    }
                }
            }
            sig = notif_stream.next() => {
                let Some(sig) = sig else { break; };
                if let Ok(args) = sig.args() {
                    if let Some(val) = args.changed_properties.get("Value") {
                        if let Some(bytes) = extract_bytes_from_value(val) {
                            let _ = event_tx.send(BluezAncsEvent::NotificationSourcePacket(bytes));
                        }
                    }
                }
            }
            sig = dev_stream.next() => {
                let Some(sig) = sig else { break; };
                if let Ok(args) = sig.args() {
                    if let Some(Value::Bool(false)) = args.changed_properties.get("Connected") {
                        bail!("Bluetooth device disconnected");
                    }
                }
            }
            Some(payload) = cp_rx.recv() => {
                let opts: HashMap<&str, Value<'_>> = HashMap::from([("type", Value::from("request"))]);
                if let Err(err) = conn
                    .call_method(
                        Some("org.bluez"),
                        ep.control_point_path.as_str(),
                        Some("org.bluez.GattCharacteristic1"),
                        "WriteValue",
                        &(payload, opts),
                    )
                    .await
                {
                    let _ = event_tx.send(BluezAncsEvent::Status(format!(
                        "ANCS Control Point write failed: {err}"
                    )));
                }
            }
        }
    }

    let _ = conn
        .call_method(
            Some("org.bluez"),
            ep.notif_source_path.as_str(),
            Some("org.bluez.GattCharacteristic1"),
            "StopNotify",
            &(),
        )
        .await;
    let _ = conn
        .call_method(
            Some("org.bluez"),
            ep.data_source_path.as_str(),
            Some("org.bluez.GattCharacteristic1"),
            "StopNotify",
            &(),
        )
        .await;

    Ok(())
}

fn find_first_adapter(objects: &ManagedObjects) -> Option<String> {
    objects
        .iter()
        .find(|(_path, ifaces)| ifaces.contains_key("org.bluez.Adapter1"))
        .map(|(path, _)| path.as_str().to_string())
}

fn summarize_pairing_hint(objects: &ManagedObjects) -> String {
    let mut connected_names = Vec::new();
    let mut paired_names = Vec::new();
    for ifaces in objects.values() {
        if let Some(dev) = ifaces.get("org.bluez.Device1") {
            let name = owned_str(dev.get("Alias"))
                .or_else(|| owned_str(dev.get("Name")))
                .unwrap_or("Bluetooth Device")
                .to_string();
            let connected = owned_bool(dev.get("Connected")).unwrap_or(false);
            let paired = owned_bool(dev.get("Paired")).unwrap_or(false);
            if connected {
                connected_names.push(name);
            } else if paired {
                paired_names.push(name);
            }
        }
    }
    if !connected_names.is_empty() {
        format!(
            "BLE connected ({}), waiting for ANCS GATT — enable 'Share System Notifications' on iPhone",
            connected_names.join(", ")
        )
    } else if !paired_names.is_empty() {
        format!(
            "Bluetooth paired ({}), waiting for BLE link…",
            paired_names.join(", ")
        )
    } else {
        "Bluetooth active; pair iPhone via Settings > Bluetooth to enable ANCS".into()
    }
}

fn find_ancs_endpoints(objects: &ManagedObjects) -> Option<AncsDeviceEndpoints> {
    let mut devices: Vec<(String, String)> = Vec::new();
    for (path, ifaces) in objects {
        if let Some(dev) = ifaces.get("org.bluez.Device1") {
            let connected = owned_bool(dev.get("Connected")).unwrap_or(false);
            let resolved = owned_bool(dev.get("ServicesResolved")).unwrap_or(false);
            if connected && resolved {
                let name = owned_str(dev.get("Alias"))
                    .or_else(|| owned_str(dev.get("Name")))
                    .unwrap_or("iPhone")
                    .to_string();
                devices.push((path.as_str().to_string(), name));
            }
        }
    }

    for (dev_path, device_name) in devices {
        let prefix = format!("{dev_path}/");
        let mut notif_path = None;
        let mut data_path = None;
        let mut cp_path = None;

        for (obj_path, ifaces) in objects {
            let p = obj_path.as_str();
            if !p.starts_with(&prefix) {
                continue;
            }
            if let Some(ch) = ifaces.get("org.bluez.GattCharacteristic1") {
                if let Some(uuid) = owned_str(ch.get("UUID")) {
                    if uuid.eq_ignore_ascii_case(ANCS_NOTIFICATION_SOURCE_UUID) {
                        notif_path = Some(p.to_string());
                    } else if uuid.eq_ignore_ascii_case(ANCS_DATA_SOURCE_UUID) {
                        data_path = Some(p.to_string());
                    } else if uuid.eq_ignore_ascii_case(ANCS_CONTROL_POINT_UUID) {
                        cp_path = Some(p.to_string());
                    }
                }
            }
        }

        if let (Some(notif_source_path), Some(data_source_path), Some(control_point_path)) =
            (notif_path, data_path, cp_path)
        {
            return Some(AncsDeviceEndpoints {
                device_name,
                device_path: dev_path,
                notif_source_path,
                data_source_path,
                control_point_path,
            });
        }
    }
    None
}

fn owned_bool(v: Option<&OwnedValue>) -> Option<bool> {
    match v.map(|x| &**x) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn owned_str(v: Option<&OwnedValue>) -> Option<&str> {
    match v.map(|x| &**x) {
        Some(Value::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn extract_bytes_from_value(v: &Value<'_>) -> Option<Vec<u8>> {
    match v {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr.inner() {
                if let Value::U8(b) = item {
                    out.push(*b);
                }
            }
            Some(out)
        }
        Value::Value(inner) => extract_bytes_from_value(inner),
        _ => None,
    }
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
    use zbus::names::OwnedInterfaceName;

    #[test]
    fn parses_bluetoothctl_powered_output() {
        let sample = "Controller 00:1A:7D:DA:71:13 (public)\n\tName: archlinux\n\tPowered: yes\n";
        assert!(parse_powered_from_bluetoothctl(sample));
        assert!(!parse_powered_from_bluetoothctl("\tPowered: no\n"));
    }

    #[test]
    fn finds_ancs_characteristics_for_connected_device() {
        let mut objs: ManagedObjects = HashMap::new();
        let dev_path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").unwrap();
        let mut dev_ifaces = HashMap::new();
        let mut dev_props: HashMap<String, OwnedValue> = HashMap::new();
        dev_props.insert("Connected".into(), OwnedValue::from(true));
        dev_props.insert("ServicesResolved".into(), OwnedValue::from(true));
        dev_props.insert(
            "Alias".into(),
            Value::from("Ar iPhone").try_to_owned().unwrap(),
        );
        dev_ifaces.insert(
            OwnedInterfaceName::try_from("org.bluez.Device1").unwrap(),
            dev_props,
        );
        objs.insert(dev_path, dev_ifaces);

        for (suffix, uuid) in [
            ("char0001", ANCS_NOTIFICATION_SOURCE_UUID),
            ("char0002", ANCS_DATA_SOURCE_UUID),
            ("char0003", ANCS_CONTROL_POINT_UUID),
        ] {
            let cpath = OwnedObjectPath::try_from(format!(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/service0010/{suffix}"
            ))
            .unwrap();
            let mut cifaces = HashMap::new();
            let mut cprops: HashMap<String, OwnedValue> = HashMap::new();
            cprops.insert(
                "UUID".into(),
                Value::from(uuid.to_ascii_lowercase())
                    .try_to_owned()
                    .unwrap(),
            );
            cifaces.insert(
                OwnedInterfaceName::try_from("org.bluez.GattCharacteristic1").unwrap(),
                cprops,
            );
            objs.insert(cpath, cifaces);
        }

        let found = find_ancs_endpoints(&objs).expect("should match all 3 ANCS characteristics");
        assert_eq!(found.device_name, "Ar iPhone");
        assert!(found.notif_source_path.ends_with("char0001"));
        assert!(found.data_source_path.ends_with("char0002"));
        assert!(found.control_point_path.ends_with("char0003"));
    }
}

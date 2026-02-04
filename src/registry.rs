use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub name: String,
    pub vid: String,
    pub pid: String,
    #[serde(default)]
    pub serial: String,
    pub on_attach: String,
    #[serde(default)]
    pub on_detach: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceEntry>,
}

#[derive(Debug)]
pub struct UsbIdentity {
    pub vid: String,
    pub pid: String,
    pub serial: String,
    pub sysname: String,
}

pub fn registry_path() -> PathBuf {
    let config = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    config.join("rapid_driver").join("registry.toml")
}

pub fn load_registry() -> Result<Registry, String> {
    let path = registry_path();
    if !path.exists() {
        return Ok(Registry::default());
    }
    let content = fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    toml::from_str(&content).map_err(|e| format!("failed to parse {}: {}", path.display(), e))
}

pub fn save_registry(registry: &Registry) -> Result<(), String> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed to create directory {}: {}", parent.display(), e))?;
    }
    let content = toml::to_string_pretty(registry).map_err(|e| format!("failed to serialize registry: {}", e))?;
    fs::write(&path, content).map_err(|e| format!("failed to write {}: {}", path.display(), e))
}

pub fn extract_identity(device: &udev::Device) -> Option<UsbIdentity> {
    let vid = device.property_value("ID_VENDOR_ID")?.to_str()?.to_string();
    let pid = device.property_value("ID_MODEL_ID")?.to_str()?.to_string();
    let serial = device
        .property_value("ID_SERIAL_SHORT")
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_string();
    let sysname = device.sysname().to_string_lossy().into_owned();
    Some(UsbIdentity { vid, pid, serial, sysname })
}

pub fn find_match<'a>(registry: &'a Registry, identity: &UsbIdentity) -> Option<&'a DeviceEntry> {
    // First pass: exact match (VID + PID + non-empty serial)
    for entry in &registry.devices {
        if entry.vid == identity.vid
            && entry.pid == identity.pid
            && !entry.serial.is_empty()
            && entry.serial == identity.serial
        {
            return Some(entry);
        }
    }
    // Second pass: model match (VID + PID, registry serial is empty)
    for entry in &registry.devices {
        if entry.vid == identity.vid && entry.pid == identity.pid && entry.serial.is_empty() {
            return Some(entry);
        }
    }
    None
}

impl Registry {
    /// Get all device names sorted alphabetically.
    pub fn device_names_sorted(&self) -> Vec<String> {
        let mut names: Vec<String> = self.devices.iter().map(|e| e.name.clone()).collect();
        names.sort();
        names
    }
}

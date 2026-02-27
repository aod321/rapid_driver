//! Mask layout configuration.
//!
//! Manages the mapping between device names and bit positions in the mask.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Configuration file for mask layout
fn layout_path() -> PathBuf {
    let config = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    config.join("rapid_driver").join("mask_layout.toml")
}

/// Mask layout configuration.
///
/// Maps device names to their bit positions in the hardware mask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaskLayout {
    /// Ordered list of device names (index = bit position)
    #[serde(default)]
    pub device_order: Vec<String>,
}

impl Default for MaskLayout {
    fn default() -> Self {
        Self {
            device_order: Vec::new(),
        }
    }
}

impl MaskLayout {
    /// Create a new layout with devices sorted alphabetically.
    pub fn from_devices_sorted(device_names: &[String]) -> Self {
        let mut names: Vec<String> = device_names.to_vec();
        names.sort();
        Self {
            device_order: names,
        }
    }

    /// Load layout from config file, or create default from device names.
    pub fn load_or_default(device_names: &[String]) -> Self {
        match Self::load() {
            Ok(mut layout) => {
                // Merge any new devices not in the saved layout
                for name in device_names {
                    if !layout.device_order.contains(name) {
                        layout.device_order.push(name.clone());
                    }
                }
                // Remove devices that no longer exist
                layout.device_order.retain(|n| device_names.contains(n));
                layout
            }
            Err(_) => Self::from_devices_sorted(device_names),
        }
    }

    /// Load layout from the config file.
    pub fn load() -> Result<Self, String> {
        let path = layout_path();
        if !path.exists() {
            return Err("Layout file does not exist".into());
        }
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        toml::from_str(&content).map_err(|e| format!("Failed to parse {}: {}", path.display(), e))
    }

    /// Save layout to the config file.
    pub fn save(&self) -> io::Result<()> {
        let path = layout_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content =
            toml::to_string_pretty(self).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(&path, content)
    }

    /// Export layout to a custom path.
    pub fn export<P: AsRef<std::path::Path>>(&self, path: P) -> io::Result<()> {
        let content =
            toml::to_string_pretty(self).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(path, content)
    }

    /// Import layout from a custom path.
    pub fn import<P: AsRef<std::path::Path>>(path: P) -> Result<Self, String> {
        let content =
            fs::read_to_string(path.as_ref()).map_err(|e| format!("Failed to read file: {}", e))?;
        toml::from_str(&content).map_err(|e| format!("Failed to parse: {}", e))
    }

    /// Get the bit index for a device name.
    ///
    /// Returns `None` if the device is not in the layout.
    pub fn bit_index(&self, device_name: &str) -> Option<usize> {
        self.device_order.iter().position(|n| n == device_name)
    }

    /// Build a lookup map from device name to bit index.
    pub fn build_index_map(&self) -> HashMap<String, usize> {
        self.device_order
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect()
    }

    /// Get the number of devices in the layout.
    pub fn device_count(&self) -> usize {
        self.device_order.len()
    }

    /// Move a device from one position to another.
    pub fn move_device(&mut self, from: usize, to: usize) {
        if from >= self.device_order.len() || to >= self.device_order.len() {
            return;
        }
        let device = self.device_order.remove(from);
        self.device_order.insert(to, device);
    }

    /// Reset to alphabetical order.
    pub fn reset_to_alphabetical(&mut self) {
        self.device_order.sort();
    }

    /// Get the config file path.
    pub fn config_path() -> PathBuf {
        layout_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_devices_sorted() {
        let names = vec!["camera".into(), "lidar".into(), "arm".into()];
        let layout = MaskLayout::from_devices_sorted(&names);
        assert_eq!(layout.device_order, vec!["arm", "camera", "lidar"]);
    }

    #[test]
    fn test_bit_index() {
        let layout = MaskLayout {
            device_order: vec!["a".into(), "b".into(), "c".into()],
        };
        assert_eq!(layout.bit_index("a"), Some(0));
        assert_eq!(layout.bit_index("b"), Some(1));
        assert_eq!(layout.bit_index("c"), Some(2));
        assert_eq!(layout.bit_index("d"), None);
    }

    #[test]
    fn test_move_device() {
        let mut layout = MaskLayout {
            device_order: vec!["a".into(), "b".into(), "c".into()],
        };
        layout.move_device(0, 2);
        assert_eq!(layout.device_order, vec!["b", "c", "a"]);
    }
}

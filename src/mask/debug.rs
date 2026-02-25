//! Debug output for hardware mask.
//!
//! Publishes a JSON file at 1Hz for debugging and human inspection.

use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

/// Debug output file path
#[cfg(target_os = "linux")]
pub const DEBUG_PATH: &str = "/dev/shm/rapid_hardware_mask.json";
#[cfg(not(target_os = "linux"))]
pub const DEBUG_PATH: &str = "/tmp/rapid_hardware_mask.json";

/// Debug state structure for JSON output.
#[derive(Debug, Clone, Serialize)]
pub struct DebugState {
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// Total device count
    pub device_count: usize,
    /// Online device count
    pub online_count: usize,
    /// Mask value in hex
    pub mask_hex: String,
    /// Mask value in binary
    pub mask_binary: String,
    /// Sequence number
    pub sequence: u64,
    /// Per-device status
    pub devices: HashMap<String, DeviceDebugInfo>,
}

/// Debug info for a single device.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceDebugInfo {
    /// Bit position in mask
    pub bit: usize,
    /// Whether device is online
    pub online: bool,
    /// USB connected (physical presence)
    pub usb_connected: bool,
    /// Process running (on_attach process alive)
    pub process_running: bool,
    /// Process ID if running
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// Publisher for debug JSON output.
pub struct DebugPublisher {
    file: File,
    last_publish: Instant,
    /// Minimum interval between publishes
    interval_ms: u64,
}

impl DebugPublisher {
    /// Create a new DebugPublisher with 1Hz (1000ms) interval.
    pub fn new() -> io::Result<Self> {
        Self::with_interval(1000)
    }

    /// Create a new DebugPublisher with a custom interval.
    pub fn with_interval(interval_ms: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(DEBUG_PATH)?;

        Ok(Self {
            file,
            last_publish: Instant::now() - std::time::Duration::from_secs(10),
            interval_ms,
        })
    }

    /// Check if it's time to publish (based on interval).
    pub fn should_publish(&self) -> bool {
        self.last_publish.elapsed().as_millis() as u64 >= self.interval_ms
    }

    /// Publish debug state if interval has elapsed.
    ///
    /// Returns `true` if published, `false` if skipped due to rate limiting.
    pub fn publish(&mut self, state: &DebugState) -> io::Result<bool> {
        if !self.should_publish() {
            return Ok(false);
        }

        self.last_publish = Instant::now();

        let json = serde_json::to_string_pretty(state)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        use std::io::Seek;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.set_len(0)?; // Truncate
        self.file.write_all(json.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;

        Ok(true)
    }

    /// Remove the debug file.
    pub fn cleanup() {
        let _ = std::fs::remove_file(DEBUG_PATH);
    }
}

impl DebugState {
    /// Create a new debug state.
    pub fn new(sequence: u64) -> Self {
        let timestamp = chrono::Utc::now().to_rfc3339();
        Self {
            timestamp,
            device_count: 0,
            online_count: 0,
            mask_hex: "0x0".into(),
            mask_binary: "0b0".into(),
            sequence,
            devices: HashMap::new(),
        }
    }

    /// Update mask-related fields.
    pub fn set_mask(&mut self, mask: u64, device_count: usize) {
        self.device_count = device_count;
        self.online_count = mask.count_ones() as usize;
        self.mask_hex = format!("{:#010x}", mask);
        self.mask_binary = format!("{:#066b}", mask);
    }

    /// Add device info.
    pub fn add_device(&mut self, name: String, info: DeviceDebugInfo) {
        self.devices.insert(name, info);
    }
}

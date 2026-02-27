//! Device discovery abstraction layer.
//!
//! Provides a unified interface for different device discovery backends (USB, mDNS, etc).

pub mod mdns;
#[cfg(target_os = "linux")]
pub mod usb;

use crate::registry::DeviceEntry;

/// A discovered device ready to be tracked.
pub struct DiscoveredDevice {
    /// Backend-internal unique identifier (USB: sysname, mDNS: fullname)
    pub source_id: String,
    /// Matched registry device name
    pub device_name: String,
    /// The matching registry entry
    pub entry: DeviceEntry,
    /// Resolved network address (e.g. "192.168.1.100:8080" for mDNS devices)
    pub address: Option<String>,
}

/// Events produced by discovery backends.
pub enum DiscoveryEvent {
    Added(DiscoveredDevice),
    Removed { source_id: String },
}

/// Unified interface for device discovery backends.
pub trait DiscoverySource: Send {
    /// Backend type identifier ("usb", "mdns", etc.)
    fn backend_type(&self) -> &str;

    /// Enumerate currently online devices.
    fn enumerate(&mut self) -> Vec<DiscoveredDevice>;

    /// Non-blocking poll for new events.
    fn poll(&mut self) -> Vec<DiscoveryEvent>;

    /// Check if a device is still online.
    fn device_still_exists(&self, source_id: &str) -> bool;

    /// Shutdown the backend.
    fn shutdown(&mut self);
}

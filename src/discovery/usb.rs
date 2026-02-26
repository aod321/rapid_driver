//! USB device discovery via udev (Linux only).

use crate::discovery::{DiscoveredDevice, DiscoveryEvent, DiscoverySource};
use crate::registry::{self, Registry};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const COOLDOWN_SECS: u64 = 2;

pub struct UsbDiscovery {
    monitor: udev::MonitorSocket,
    registry: Registry,
    cooldown: HashMap<String, Instant>,
    known_devices: HashMap<String, String>, // sysname -> device_name
}

impl UsbDiscovery {
    pub fn new(registry: Registry) -> Self {
        let monitor = udev::MonitorBuilder::new()
            .expect("failed to create monitor builder")
            .match_subsystem_devtype("usb", "usb_device")
            .expect("failed to set subsystem filter")
            .listen()
            .expect("failed to listen on monitor");

        Self {
            monitor,
            registry,
            cooldown: HashMap::new(),
            known_devices: HashMap::new(),
        }
    }

    /// Get the monitor socket fd for polling.
    pub fn monitor_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
        use std::os::unix::io::AsFd;
        self.monitor.as_fd()
    }
}

impl DiscoverySource for UsbDiscovery {
    fn backend_type(&self) -> &str {
        "usb"
    }

    fn enumerate(&mut self) -> Vec<DiscoveredDevice> {
        let mut found = Vec::new();
        let mut enumerator = udev::Enumerator::new().expect("failed to create enumerator");
        enumerator.match_subsystem("usb").expect("failed to match subsystem");

        let devices = enumerator.scan_devices().expect("failed to scan devices");
        for device in devices {
            let devtype = device.property_value("DEVTYPE");
            if devtype.and_then(|v| v.to_str()) != Some("usb_device") {
                continue;
            }
            if let Some(identity) = registry::extract_identity(&device) {
                if let Some(entry) = registry::find_match(&self.registry, &identity).cloned() {
                    self.known_devices.insert(identity.sysname.clone(), entry.name.clone());
                    found.push(DiscoveredDevice {
                        source_id: identity.sysname,
                        device_name: entry.name.clone(),
                        entry,
                        address: None,
                    });
                }
            }
        }
        found
    }

    fn poll(&mut self) -> Vec<DiscoveryEvent> {
        let mut events = Vec::new();

        // Non-blocking: iterate available events
        use std::os::unix::io::AsFd;
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

        let fd = self.monitor.as_fd();
        let mut poll_fds = [PollFd::new(fd, PollFlags::POLLIN)];
        if let Ok(n) = poll(&mut poll_fds, PollTimeout::from(0u16)) {
            if n > 0 {
                if let Some(event) = self.monitor.iter().next() {
                    let action = event.action().and_then(|a| a.to_str()).unwrap_or("unknown");
                    let device = event.device();

                    let devtype = device.property_value("DEVTYPE");
                    if devtype.and_then(|v| v.to_str()) != Some("usb_device") {
                        return events;
                    }

                    let sysname = device.sysname().to_string_lossy().into_owned();

                    match action {
                        "add" => {
                            // Cooldown check
                            if let Some(&last_remove) = self.cooldown.get(&sysname) {
                                if last_remove.elapsed() < Duration::from_secs(COOLDOWN_SECS) {
                                    return events;
                                }
                                self.cooldown.remove(&sysname);
                            }
                            if let Some(identity) = registry::extract_identity(&device) {
                                if let Some(entry) = registry::find_match(&self.registry, &identity).cloned() {
                                    self.known_devices.insert(identity.sysname.clone(), entry.name.clone());
                                    events.push(DiscoveryEvent::Added(DiscoveredDevice {
                                        source_id: identity.sysname,
                                        device_name: entry.name.clone(),
                                        entry,
                                        address: None,
                                    }));
                                }
                            }
                        }
                        "remove" => {
                            if self.known_devices.remove(&sysname).is_some() {
                                self.cooldown.insert(sysname.clone(), Instant::now());
                                events.push(DiscoveryEvent::Removed { source_id: sysname });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        events
    }

    fn device_still_exists(&self, source_id: &str) -> bool {
        let path = format!("/sys/bus/usb/devices/{}", source_id);
        std::path::Path::new(&path).exists()
    }

    fn shutdown(&mut self) {
        // Nothing specific to clean up for udev
    }
}

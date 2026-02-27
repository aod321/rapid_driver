//! mDNS/Bonjour device discovery via mdns-sd.

use crate::discovery::{DiscoveredDevice, DiscoveryEvent, DiscoverySource};
use crate::registry::{DeviceEntry, Registry};
use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// How long a newly-discovered service must remain visible before we report Added.
const ADD_SETTLE_SECS: u64 = 2;

/// How long after ServiceRemoved before we confirm the device is gone.
const REMOVAL_GRACE_SECS: u64 = 5;

/// A service that was resolved but not yet confirmed.
struct PendingAdd {
    discovered: DiscoveredDevice,
    first_seen: Instant,
    /// Set to true when ServiceRemoved arrives during settle; reset on re-Resolved.
    removed: bool,
}

pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
    receivers: Vec<(String, Receiver<ServiceEvent>)>,
    registry: Registry,
    /// Confirmed-online services: fullname -> (device_name, entry, address)
    known_services: HashMap<String, (String, DeviceEntry, Option<String>)>,
    /// Tracked service types
    browsed_types: HashSet<String>,
    /// Services waiting to be confirmed as Added
    pending_adds: HashMap<String, PendingAdd>,
    /// Services waiting to be confirmed as Removed
    pending_removals: HashMap<String, Instant>,
}

impl MdnsDiscovery {
    pub fn new(registry: Registry) -> Self {
        let daemon = ServiceDaemon::new().expect("failed to create mDNS daemon");
        let mut discovery = Self {
            daemon,
            receivers: Vec::new(),
            registry,
            known_services: HashMap::new(),
            browsed_types: HashSet::new(),
            pending_adds: HashMap::new(),
            pending_removals: HashMap::new(),
        };

        let service_types: Vec<String> = discovery
            .registry
            .devices
            .iter()
            .filter(|e| e.backend == "mdns" && !e.service_type.is_empty())
            .map(|e| normalize_service_type(&e.service_type))
            .collect();

        for st in service_types {
            if discovery.browsed_types.contains(&st) {
                continue;
            }
            if let Ok(receiver) = discovery.daemon.browse(&st) {
                discovery.browsed_types.insert(st.clone());
                discovery.receivers.push((st, receiver));
            }
        }

        discovery
    }

    fn find_mdns_match(&self, service_type: &str) -> Option<&DeviceEntry> {
        self.registry.devices.iter().find(|e| {
            if e.backend != "mdns" || e.service_type.is_empty() {
                return false;
            }
            let entry_st = normalize_service_type(&e.service_type);
            let norm_entry = entry_st.trim_end_matches('.');
            let norm_discovered = service_type.trim_end_matches('.');
            norm_entry == norm_discovered
        })
    }
}

fn normalize_service_type(st: &str) -> String {
    let mut s = st.to_string();
    if !s.ends_with(".local.") {
        if s.ends_with('.') {
            s.push_str("local.");
        } else {
            s.push_str(".local.");
        }
    }
    s
}

impl DiscoverySource for MdnsDiscovery {
    fn backend_type(&self) -> &str {
        "mdns"
    }

    fn enumerate(&mut self) -> Vec<DiscoveredDevice> {
        // mDNS initial cache is unreliable; don't enumerate.
        // Let poll() handle discovery after the settle period.
        Vec::new()
    }

    fn poll(&mut self) -> Vec<DiscoveryEvent> {
        let mut events = Vec::new();

        // 1. Drain mDNS events
        for (_service_type, receiver) in &self.receivers {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let fullname = info.get_fullname().to_string();
                        let service_type = info.get_type().to_string();

                        // Cancel pending removal if service reappears
                        if self.pending_removals.remove(&fullname).is_some() {
                            continue;
                        }

                        // Already confirmed
                        if self.known_services.contains_key(&fullname) {
                            continue;
                        }

                        // Re-resolved during settle period: mark as alive again
                        if let Some(pa) = self.pending_adds.get_mut(&fullname) {
                            pa.removed = false;
                            continue;
                        }

                        if let Some(entry) = self.find_mdns_match(&service_type).cloned() {
                            let data_port = info
                                .get_property_val_str("data_port")
                                .and_then(|v| v.parse::<u16>().ok())
                                .unwrap_or_else(|| info.get_port());
                            let address = info
                                .get_addresses()
                                .iter()
                                .next()
                                .map(|ip| format!("{}:{}", ip, data_port));
                            self.pending_adds.insert(
                                fullname.clone(),
                                PendingAdd {
                                    discovered: DiscoveredDevice {
                                        source_id: fullname,
                                        device_name: entry.name.clone(),
                                        entry,
                                        address,
                                    },
                                    first_seen: Instant::now(),
                                    removed: false,
                                },
                            );
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        // If pending add, mark as removed (don't delete — may re-resolve)
                        if let Some(pa) = self.pending_adds.get_mut(&fullname) {
                            pa.removed = true;
                            continue;
                        }
                        // If confirmed, start removal grace period
                        if self.known_services.contains_key(&fullname) {
                            self.pending_removals
                                .entry(fullname)
                                .or_insert_with(Instant::now);
                        }
                    }
                    _ => {}
                }
            }
        }

        // 2. Check settled pending adds
        let settle = std::time::Duration::from_secs(ADD_SETTLE_SECS);
        let mut promoted = Vec::new();
        let mut discarded = Vec::new();
        for (fullname, pa) in &self.pending_adds {
            if pa.first_seen.elapsed() >= settle {
                if pa.removed {
                    discarded.push(fullname.clone()); // Stale cache — gone during settle
                } else {
                    promoted.push(fullname.clone()); // Still alive — real device
                }
            }
        }
        for fullname in &discarded {
            self.pending_adds.remove(fullname);
        }
        for fullname in promoted {
            if let Some(pa) = self.pending_adds.remove(&fullname) {
                let d = &pa.discovered;
                self.known_services.insert(
                    fullname,
                    (d.device_name.clone(), d.entry.clone(), d.address.clone()),
                );
                events.push(DiscoveryEvent::Added(pa.discovered));
            }
        }

        // 3. Expire pending removals → confirmed + emit Removed
        let grace = std::time::Duration::from_secs(REMOVAL_GRACE_SECS);
        let mut expired = Vec::new();
        for (fullname, since) in &self.pending_removals {
            if since.elapsed() >= grace {
                expired.push(fullname.clone());
            }
        }
        for fullname in expired {
            self.pending_removals.remove(&fullname);
            if self.known_services.remove(&fullname).is_some() {
                events.push(DiscoveryEvent::Removed {
                    source_id: fullname,
                });
            }
        }

        events
    }

    fn device_still_exists(&self, source_id: &str) -> bool {
        self.known_services.contains_key(source_id) || self.pending_removals.contains_key(source_id)
    }

    fn shutdown(&mut self) {
        for (st, _) in &self.receivers {
            let _ = self.daemon.stop_browse(st);
        }
        let _ = self.daemon.shutdown();
    }
}

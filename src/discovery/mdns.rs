//! mDNS/Bonjour device discovery via mdns-sd.

use crate::discovery::{DiscoveredDevice, DiscoveryEvent, DiscoverySource};
use crate::registry::{DeviceEntry, Registry};
use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};
use std::collections::{HashMap, HashSet};

pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
    receivers: Vec<(String, Receiver<ServiceEvent>)>, // (service_type, receiver)
    registry: Registry,
    /// Map: service fullname -> (device_name, entry, address)
    known_services: HashMap<String, (String, DeviceEntry, Option<String>)>,
    /// Tracked service types
    browsed_types: HashSet<String>,
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
        };

        // Start browsing for each mDNS device entry's service type
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

    /// Find a registry entry matching a discovered service type.
    fn find_mdns_match(&self, service_type: &str) -> Option<&DeviceEntry> {
        self.registry.devices.iter().find(|e| {
            if e.backend != "mdns" || e.service_type.is_empty() {
                return false;
            }
            let entry_st = normalize_service_type(&e.service_type);
            // Normalize: compare without trailing dot differences
            let norm_entry = entry_st.trim_end_matches('.');
            let norm_discovered = service_type.trim_end_matches('.');
            norm_entry == norm_discovered
        })
    }
}

/// Normalize a service type to end with ".local."
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
        // Process any already-received events to populate known_services
        let _ = self.poll();
        self.known_services
            .iter()
            .map(|(fullname, (device_name, entry, address))| DiscoveredDevice {
                source_id: fullname.clone(),
                device_name: device_name.clone(),
                entry: entry.clone(),
                address: address.clone(),
            })
            .collect()
    }

    fn poll(&mut self) -> Vec<DiscoveryEvent> {
        let mut events = Vec::new();
        for (_service_type, receiver) in &self.receivers {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let fullname = info.get_fullname().to_string();
                        let service_type = info.get_type().to_string();
                        if self.known_services.contains_key(&fullname) {
                            continue; // Already known
                        }
                        if let Some(entry) = self.find_mdns_match(&service_type).cloned() {
                            // Extract resolved address (IP:port)
                            let address = info.get_addresses().iter().next().map(|ip| {
                                format!("{}:{}", ip, info.get_port())
                            });
                            self.known_services.insert(
                                fullname.clone(),
                                (entry.name.clone(), entry.clone(), address.clone()),
                            );
                            events.push(DiscoveryEvent::Added(DiscoveredDevice {
                                source_id: fullname,
                                device_name: entry.name.clone(),
                                entry,
                                address,
                            }));
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        if self.known_services.remove(&fullname).is_some() {
                            events.push(DiscoveryEvent::Removed {
                                source_id: fullname,
                            });
                        }
                    }
                    _ => {} // Ignore SearchStarted, SearchStopped, etc.
                }
            }
        }
        events
    }

    fn device_still_exists(&self, source_id: &str) -> bool {
        self.known_services.contains_key(source_id)
    }

    fn shutdown(&mut self) {
        for (st, _) in &self.receivers {
            let _ = self.daemon.stop_browse(st);
        }
        let _ = self.daemon.shutdown();
    }
}

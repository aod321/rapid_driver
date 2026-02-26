mod api;
mod discovery;
mod engine;
mod mask;
mod registry;
mod tui;
#[cfg(target_os = "linux")]
mod udev_rule;

use clap::{Parser, Subcommand};
use engine::command::{EngineCommand, EngineHandle};
use engine::Engine;
use mask::MaskLayout;
use registry::DeviceEntry;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;

#[derive(Parser)]
#[command(name = "rapid_driver", about = "Device-triggered process manager", infer_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Load registry and monitor USB hotplug events (default)
    #[command(visible_alias = "mon")]
    Monitor {
        /// Disable hardware mask publishing to shared memory
        #[arg(long)]
        no_mask: bool,
        /// Enable HTTP API server (e.g. --api 0.0.0.0:7400)
        #[arg(long)]
        api: Option<String>,
    },
    /// Headless daemon with HTTP API server
    #[command(visible_alias = "srv")]
    Serve {
        /// API server bind address
        #[arg(long, default_value = "0.0.0.0:7400")]
        bind: String,
        /// Disable hardware mask publishing to shared memory
        #[arg(long)]
        no_mask: bool,
    },
    /// Interactively register a USB device and its command
    #[command(visible_alias = "reg")]
    Register,
    /// List all registered devices
    #[command(visible_alias = "ls")]
    List,
    /// Unregister a device by name
    #[command(visible_alias = "unreg")]
    Unregister { name: String },
    /// View device logs
    Logs {
        /// Device name (show all if omitted)
        name: Option<String>,
        /// Number of lines to show (default: 20)
        #[arg(short = 'n', long, default_value = "20")]
        lines: usize,
        /// Follow log output (like tail -f)
        #[arg(short, long)]
        follow: bool,
    },
    /// Manage hardware mask bit layout
    #[command(visible_alias = "ml")]
    MaskLayout {
        #[command(subcommand)]
        action: MaskLayoutAction,
    },
}

#[derive(Subcommand)]
enum MaskLayoutAction {
    /// Show current layout
    Show,
    /// Export layout to file
    Export {
        /// Output file path
        path: PathBuf,
    },
    /// Import layout from file
    Import {
        /// Input file path
        path: PathBuf,
    },
    /// Reset to alphabetical order
    Reset,
}

fn main() {
    let cli = Cli::parse();

    match cli
        .command
        .unwrap_or(Commands::Monitor { no_mask: false, api: None })
    {
        Commands::Monitor { no_mask, api } => cmd_monitor(!no_mask, api),
        Commands::Serve { bind, no_mask } => cmd_serve(&bind, !no_mask),
        Commands::Register => cmd_register(),
        Commands::List => cmd_list(),
        Commands::Unregister { name } => cmd_unregister(&name),
        Commands::Logs {
            name,
            lines,
            follow,
        } => cmd_logs(name, lines, follow),
        Commands::MaskLayout { action } => cmd_mask_layout(action),
    }
}

/// Build discovery sources from registry.
fn build_discovery_sources(
    registry: &registry::Registry,
) -> Vec<Box<dyn discovery::DiscoverySource>> {
    let mut sources: Vec<Box<dyn discovery::DiscoverySource>> = Vec::new();

    #[cfg(target_os = "linux")]
    {
        if registry.devices.iter().any(|e| e.backend == "usb") {
            sources.push(Box::new(discovery::usb::UsbDiscovery::new(
                registry.clone(),
            )));
        }
    }

    if registry.devices.iter().any(|e| e.backend == "mdns") {
        sources.push(Box::new(discovery::mdns::MdnsDiscovery::new(
            registry.clone(),
        )));
    }

    sources
}

/// Create engine + handle pair, spawn engine thread.
fn spawn_engine(
    reg: registry::Registry,
    mask_enabled: bool,
) -> (EngineHandle, std::thread::JoinHandle<()>) {
    let sources = build_discovery_sources(&reg);
    let (cmd_tx, cmd_rx) = mpsc::sync_channel(64);
    let handle = EngineHandle::new(cmd_tx);

    let engine_thread = std::thread::spawn(move || {
        let mut engine = Engine::new(reg, mask_enabled, sources, cmd_rx);
        engine.run();
    });

    (handle, engine_thread)
}

/// Parse port number from a "host:port" bind address string.
fn parse_port_from_bind(bind: &str) -> Option<u16> {
    bind.rsplit(':').next()?.parse().ok()
}

/// Handle for the mDNS advertising subprocess; kills it on drop.
struct MdnsAdvertiseHandle(std::process::Child);

impl Drop for MdnsAdvertiseHandle {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Advertise the HTTP API via mDNS as `_rapiddriver._tcp`.
/// Uses the platform's native tool (dns-sd on macOS, avahi on Linux).
/// Returns a handle — service stays registered as long as this is alive.
fn advertise_rapiddriver_api(port: u16) -> Option<MdnsAdvertiseHandle> {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("dns-sd")
        .arg("-R")
        .arg("rapid-driver")
        .arg("_rapiddriver._tcp")
        .arg(".")
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("avahi-publish-service")
        .args(["rapid-driver", "_rapiddriver._tcp", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result: Result<std::process::Child, std::io::Error> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mDNS advertising not supported on this platform",
    ));

    match result {
        Ok(child) => {
            eprintln!("[mDNS] Advertising _rapiddriver._tcp on port {}", port);
            Some(MdnsAdvertiseHandle(child))
        }
        Err(e) => {
            eprintln!("[mDNS] Failed to advertise _rapiddriver._tcp: {}", e);
            None
        }
    }
}

fn cmd_monitor(mask_enabled: bool, api_bind: Option<String>) {
    let reg = match registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading registry: {}", e);
            std::process::exit(1);
        }
    };

    if reg.devices.is_empty() {
        println!("Warning: registry is empty. Use 'rapid_driver register' to add devices.");
    }

    let (handle, engine_thread) = spawn_engine(reg, mask_enabled);

    // Optionally spawn API server thread + advertise via mDNS
    let _mdns_daemon = if let Some(ref bind) = api_bind {
        let api_handle = handle.clone();
        let bind_clone = bind.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            rt.block_on(async {
                let app = api::build_router(api_handle);
                let listener = tokio::net::TcpListener::bind(&bind_clone)
                    .await
                    .unwrap_or_else(|e| {
                        eprintln!("Failed to bind API to {}: {}", bind_clone, e);
                        std::process::exit(1);
                    });
                eprintln!("API server listening on {}", bind_clone);
                axum::serve(listener, app).await.unwrap();
            });
        });
        parse_port_from_bind(bind).and_then(advertise_rapiddriver_api)
    } else {
        None
    };

    // Run TUI on main thread
    if let Err(e) = tui::run_tui(handle.clone(), mask_enabled) {
        eprintln!("TUI error: {}", e);
    }

    // Signal engine shutdown
    let _ = handle.send(EngineCommand::Shutdown);
    let _ = engine_thread.join();
}

fn cmd_serve(bind: &str, mask_enabled: bool) {
    let reg = match registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading registry: {}", e);
            std::process::exit(1);
        }
    };

    if reg.devices.is_empty() {
        println!("Warning: registry is empty. Use 'rapid_driver register' to add devices.");
    }

    let (handle, engine_thread) = spawn_engine(reg, mask_enabled);

    // Advertise API via mDNS
    let _mdns_daemon = parse_port_from_bind(bind).and_then(advertise_rapiddriver_api);

    // Run API server on main thread
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let api_handle = handle.clone();
    let bind = bind.to_string();
    rt.block_on(async {
        let app = api::build_router(api_handle);
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .unwrap_or_else(|e| {
                eprintln!("Failed to bind to {}: {}", bind, e);
                std::process::exit(1);
            });
        println!("rapid_driver serving on http://{}", bind);
        println!("Press Ctrl+C to stop.");
        axum::serve(listener, app).await.unwrap();
    });

    let _ = handle.send(EngineCommand::Shutdown);
    let _ = engine_thread.join();
}

fn cmd_register() {
    let mut reg = match registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading registry: {}", e);
            std::process::exit(1);
        }
    };

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    // Ask for backend type
    println!("Select backend:");
    println!("  [1] USB (udev hotplug)");
    println!("  [2] mDNS (Bonjour/Zeroconf)");
    print!("Choice [1]: ");
    io::stdout().flush().unwrap();
    let backend_choice = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => return,
    };
    let backend = match backend_choice.as_str() {
        "" | "1" => "usb",
        "2" => "mdns",
        _ => {
            eprintln!("Invalid selection.");
            return;
        }
    };

    match backend {
        "usb" => register_usb_device(&mut reg, &mut lines),
        "mdns" => register_mdns_device(&mut reg, &mut lines),
        _ => unreachable!(),
    }
}

fn register_mdns_device(
    reg: &mut registry::Registry,
    lines: &mut std::io::Lines<std::io::StdinLock>,
) {
    print!("Enter a name for this device: ");
    io::stdout().flush().unwrap();
    let name = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => return,
    };
    if name.is_empty() {
        eprintln!("Name cannot be empty.");
        return;
    }
    if reg.devices.iter().any(|e| e.name == name) {
        eprintln!("Name '{}' already exists in registry.", name);
        return;
    }

    print!("Enter mDNS service type (e.g. _iphonevio._tcp.local.): ");
    io::stdout().flush().unwrap();
    let service_type = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => return,
    };
    if service_type.is_empty() {
        eprintln!("Service type cannot be empty.");
        return;
    }

    print!("Enter on_attach command: ");
    io::stdout().flush().unwrap();
    let on_attach = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => return,
    };
    if on_attach.is_empty() {
        eprintln!("on_attach command cannot be empty.");
        return;
    }

    print!("Enter on_detach command (optional, press Enter to skip): ");
    io::stdout().flush().unwrap();
    let on_detach = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => String::new(),
    };

    print!("Enter on_record_start command (optional): ");
    io::stdout().flush().unwrap();
    let on_record_start = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => String::new(),
    };

    print!("Enter on_record_stop command (optional): ");
    io::stdout().flush().unwrap();
    let on_record_stop = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => String::new(),
    };

    print!("Sensor type for MCAP recording (motor/video/gopro, or empty to skip): ");
    io::stdout().flush().unwrap();
    let sensor_type = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => String::new(),
    };

    let entry = DeviceEntry {
        name: name.clone(),
        backend: "mdns".to_string(),
        vid: String::new(),
        pid: String::new(),
        serial: String::new(),
        service_type,
        on_attach,
        on_detach,
        on_record_start,
        on_record_stop,
        sensor_type,
    };

    println!("\nRegistering: {:?}", entry);
    reg.devices.push(entry);

    match registry::save_registry(reg) {
        Ok(()) => println!("Saved to {}", registry::registry_path().display()),
        Err(e) => eprintln!("Error saving registry: {}", e),
    }
}

#[cfg(target_os = "linux")]
fn register_usb_device(
    reg: &mut registry::Registry,
    lines: &mut std::io::Lines<std::io::StdinLock>,
) {
    let mut enumerator = udev::Enumerator::new().expect("failed to create enumerator");
    enumerator
        .match_subsystem("usb")
        .expect("failed to match subsystem");

    let devices: Vec<_> = enumerator
        .scan_devices()
        .expect("failed to scan devices")
        .filter(|d| {
            d.property_value("DEVTYPE")
                .and_then(|v| v.to_str())
                == Some("usb_device")
        })
        .filter_map(|d| registry::extract_identity(&d).map(|id| (id, d)))
        .collect();

    if devices.is_empty() {
        println!("No USB devices found.");
        return;
    }

    println!("=== Current USB Devices ===\n");
    for (i, (identity, device)) in devices.iter().enumerate() {
        let vendor = device
            .property_value("ID_VENDOR_FROM_DATABASE")
            .or_else(|| device.property_value("ID_VENDOR"))
            .and_then(|v| v.to_str())
            .unwrap_or("N/A");
        let product = device
            .property_value("ID_MODEL_FROM_DATABASE")
            .or_else(|| device.property_value("ID_MODEL"))
            .and_then(|v| v.to_str())
            .unwrap_or("N/A");

        println!(
            "  [{}] {}:{} {} / {}  serial='{}'",
            i, identity.vid, identity.pid, vendor, product, identity.serial
        );
    }

    print!("\nSelect device number: ");
    io::stdout().flush().unwrap();
    let idx: usize = match lines.next() {
        Some(Ok(line)) => match line.trim().parse() {
            Ok(n) if n < devices.len() => n,
            _ => {
                eprintln!("Invalid selection.");
                return;
            }
        },
        _ => return,
    };

    let (identity, _) = &devices[idx];

    print!("Enter a name for this device: ");
    io::stdout().flush().unwrap();
    let name = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => return,
    };
    if name.is_empty() {
        eprintln!("Name cannot be empty.");
        return;
    }
    if reg.devices.iter().any(|e| e.name == name) {
        eprintln!("Name '{}' already exists in registry.", name);
        return;
    }

    print!("Enter on_attach command: ");
    io::stdout().flush().unwrap();
    let on_attach = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => return,
    };
    if on_attach.is_empty() {
        eprintln!("on_attach command cannot be empty.");
        return;
    }

    print!("Enter on_detach command (optional, press Enter to skip): ");
    io::stdout().flush().unwrap();
    let on_detach = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => String::new(),
    };

    let serial = if !identity.serial.is_empty() {
        print!(
            "Use serial '{}' for exact matching? (y/N): ",
            identity.serial
        );
        io::stdout().flush().unwrap();
        match lines.next() {
            Some(Ok(line)) if line.trim().eq_ignore_ascii_case("y") => identity.serial.clone(),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    print!("Create udev symlink /dev/{}? (y/N): ", name);
    io::stdout().flush().unwrap();
    let create_udev =
        matches!(lines.next(), Some(Ok(line)) if line.trim().eq_ignore_ascii_case("y"));

    let entry = DeviceEntry {
        name: name.clone(),
        backend: "usb".to_string(),
        vid: identity.vid.clone(),
        pid: identity.pid.clone(),
        serial,
        service_type: String::new(),
        on_attach,
        on_detach,
        on_record_start: String::new(),
        on_record_stop: String::new(),
        sensor_type: String::new(),
    };

    println!("\nRegistering: {:?}", entry);
    reg.devices.push(entry.clone());

    match registry::save_registry(reg) {
        Ok(()) => println!("Saved to {}", registry::registry_path().display()),
        Err(e) => eprintln!("Error saving registry: {}", e),
    }

    if create_udev {
        match udev_rule::create_udev_rule(&entry) {
            Ok(path) => {
                println!("Created udev rule: {}", path.display());
                print!("Reloading udev rules... ");
                io::stdout().flush().unwrap();
                match udev_rule::reload_udev() {
                    Ok(()) => {
                        println!("done");
                        println!("Device will be available at: /dev/{}", entry.name);
                    }
                    Err(e) => eprintln!("failed: {}", e),
                }
            }
            Err(e) => eprintln!("Failed to create udev rule: {}", e),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn register_usb_device(
    _reg: &mut registry::Registry,
    _lines: &mut std::io::Lines<std::io::StdinLock>,
) {
    eprintln!("USB device registration is only supported on Linux.");
}

fn cmd_list() {
    let reg = match registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading registry: {}", e);
            std::process::exit(1);
        }
    };

    if reg.devices.is_empty() {
        println!("Registry is empty.");
        return;
    }

    println!(
        "{:<20} {:<8} {:<10} {:<20} {:<8} {:<30} {}",
        "NAME", "BACKEND", "VID:PID", "SERVICE_TYPE", "SENSOR", "ON_ATTACH", "ON_DETACH"
    );
    println!("{}", "-".repeat(120));

    for entry in &reg.devices {
        let vid_pid = if entry.vid.is_empty() && entry.pid.is_empty() {
            "-".to_string()
        } else {
            format!("{}:{}", entry.vid, entry.pid)
        };
        let service_type = if entry.service_type.is_empty() {
            "-"
        } else {
            &entry.service_type
        };
        let sensor = if entry.sensor_type.is_empty() {
            "-"
        } else {
            &entry.sensor_type
        };
        let on_detach = if entry.on_detach.is_empty() {
            "(none)"
        } else {
            &entry.on_detach
        };
        println!(
            "{:<20} {:<8} {:<10} {:<20} {:<8} {:<30} {}",
            entry.name, entry.backend, vid_pid, service_type, sensor, entry.on_attach, on_detach
        );
    }
}

fn cmd_unregister(name: &str) {
    let mut reg = match registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading registry: {}", e);
            std::process::exit(1);
        }
    };

    let before = reg.devices.len();
    reg.devices.retain(|e| e.name != name);

    if reg.devices.len() == before {
        eprintln!("No device named '{}' found in registry.", name);
        std::process::exit(1);
    }

    match registry::save_registry(&reg) {
        Ok(()) => println!("Removed '{}' from registry.", name),
        Err(e) => eprintln!("Error saving registry: {}", e),
    }

    #[cfg(target_os = "linux")]
    {
        match udev_rule::remove_udev_rule(name) {
            Ok(()) => {
                println!("Removed udev rule for '{}'.", name);
                let _ = udev_rule::reload_udev();
            }
            Err(e) => eprintln!("Warning: {}", e),
        }
    }
}

fn cmd_logs(name: Option<String>, lines: usize, follow: bool) {
    use std::fs::{self, File};
    use std::io::{BufRead, BufReader, Seek, SeekFrom};
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    let log_dir = dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rapid_driver")
        .join("logs");

    if !log_dir.exists() {
        println!("No logs found. Run 'rapid_driver monitor' first.");
        return;
    }

    let log_files: Vec<PathBuf> = if let Some(ref device_name) = name {
        let path = log_dir.join(format!("{}.log", device_name));
        if path.exists() {
            vec![path]
        } else {
            eprintln!("No log file for device '{}'", device_name);
            return;
        }
    } else {
        match fs::read_dir(&log_dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |ext| ext == "log"))
                .collect(),
            Err(e) => {
                eprintln!("Failed to read log directory: {}", e);
                return;
            }
        }
    };

    if log_files.is_empty() {
        println!("No log files found.");
        return;
    }

    for log_path in &log_files {
        let device_name = log_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        println!("=== {} ===", device_name);

        if let Ok(file) = File::open(log_path) {
            let reader = BufReader::new(file);
            let all_lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
            let start = all_lines.len().saturating_sub(lines);
            for line in &all_lines[start..] {
                println!("{}", line);
            }
        }
        println!();
    }

    if follow {
        println!("--- Following logs (Ctrl+C to stop) ---\n");

        let mut positions: Vec<u64> = log_files
            .iter()
            .map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .collect();

        loop {
            for (i, log_path) in log_files.iter().enumerate() {
                if let Ok(mut file) = File::open(log_path) {
                    let current_len = file.metadata().map(|m| m.len()).unwrap_or(0);
                    if current_len > positions[i] {
                        let _ = file.seek(SeekFrom::Start(positions[i]));
                        let reader = BufReader::new(file);
                        let device_name = log_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("?");

                        for line in reader.lines().filter_map(|l| l.ok()) {
                            println!("[{}] {}", device_name, line);
                        }
                        positions[i] = current_len;
                    }
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

fn cmd_mask_layout(action: MaskLayoutAction) {
    let reg = match registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading registry: {}", e);
            std::process::exit(1);
        }
    };

    let device_names = reg.device_names_sorted();

    match action {
        MaskLayoutAction::Show => {
            let layout = MaskLayout::load_or_default(&device_names);

            println!("=== Hardware Mask Layout ===\n");
            println!("Config file: {}", MaskLayout::config_path().display());
            println!();
            println!("{:<4} {:<20} {}", "BIT", "DEVICE", "STATUS");
            println!("{}", "-".repeat(40));

            for (bit, name) in layout.device_order.iter().enumerate() {
                let status = if device_names.contains(name) {
                    "registered"
                } else {
                    "NOT IN REGISTRY"
                };
                println!("[{:>2}] {:<20} {}", bit, name, status);
            }

            let layout_names: std::collections::HashSet<_> = layout.device_order.iter().collect();
            let missing: Vec<_> = device_names
                .iter()
                .filter(|n| !layout_names.contains(n))
                .collect();
            if !missing.is_empty() {
                println!("\nDevices in registry but not in layout (will be added):");
                for name in missing {
                    println!("  - {}", name);
                }
            }
        }
        MaskLayoutAction::Export { path } => {
            let layout = MaskLayout::load_or_default(&device_names);
            match layout.export(&path) {
                Ok(()) => println!("Exported layout to: {}", path.display()),
                Err(e) => {
                    eprintln!("Error exporting layout: {}", e);
                    std::process::exit(1);
                }
            }
        }
        MaskLayoutAction::Import { path } => match MaskLayout::import(&path) {
            Ok(layout) => match layout.save() {
                Ok(()) => {
                    println!("Imported layout from: {}", path.display());
                    println!("Saved to: {}", MaskLayout::config_path().display());
                }
                Err(e) => {
                    eprintln!("Error saving layout: {}", e);
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("Error importing layout: {}", e);
                std::process::exit(1);
            }
        },
        MaskLayoutAction::Reset => {
            let layout = MaskLayout::from_devices_sorted(&device_names);
            match layout.save() {
                Ok(()) => {
                    println!("Reset layout to alphabetical order.");
                    println!("Saved to: {}", MaskLayout::config_path().display());
                }
                Err(e) => {
                    eprintln!("Error saving layout: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

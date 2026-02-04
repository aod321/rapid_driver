mod mask;
mod registry;
mod tui;
mod udev_rule;

use clap::{Parser, Subcommand};
use mask::MaskLayout;
use registry::DeviceEntry;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

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

    match cli.command.unwrap_or(Commands::Monitor { no_mask: false }) {
        Commands::Monitor { no_mask } => cmd_monitor(!no_mask),
        Commands::Register => cmd_register(),
        Commands::List => cmd_list(),
        Commands::Unregister { name } => cmd_unregister(&name),
        Commands::Logs { name, lines, follow } => cmd_logs(name, lines, follow),
        Commands::MaskLayout { action } => cmd_mask_layout(action),
    }
}

fn cmd_monitor(mask_enabled: bool) {
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

    if let Err(e) = tui::run_tui(reg, mask_enabled) {
        eprintln!("TUI error: {}", e);
        std::process::exit(1);
    }
}

fn cmd_register() {
    let mut reg = match registry::load_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error loading registry: {}", e);
            std::process::exit(1);
        }
    };

    // Enumerate current USB devices
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

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    // Select device
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

    // Enter name
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

    // Enter on_attach command
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

    // Enter on_detach command (optional)
    print!("Enter on_detach command (optional, press Enter to skip): ");
    io::stdout().flush().unwrap();
    let on_detach = match lines.next() {
        Some(Ok(line)) => line.trim().to_string(),
        _ => String::new(),
    };

    // Ask about serial matching
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

    // Ask about creating udev symlink
    print!("Create udev symlink /dev/{}? (y/N): ", name);
    io::stdout().flush().unwrap();
    let create_udev = matches!(lines.next(), Some(Ok(line)) if line.trim().eq_ignore_ascii_case("y"));

    let entry = DeviceEntry {
        name: name.clone(),
        vid: identity.vid.clone(),
        pid: identity.pid.clone(),
        serial,
        on_attach,
        on_detach,
    };

    println!("\nRegistering: {:?}", entry);
    reg.devices.push(entry.clone());

    match registry::save_registry(&reg) {
        Ok(()) => println!("Saved to {}", registry::registry_path().display()),
        Err(e) => eprintln!("Error saving registry: {}", e),
    }

    // Create udev rule if requested
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
        "{:<20} {:<10} {:<12} {:<30} {}",
        "NAME", "VID:PID", "SERIAL", "ON_ATTACH", "ON_DETACH"
    );
    println!("{}", "-".repeat(90));

    for entry in &reg.devices {
        let vid_pid = format!("{}:{}", entry.vid, entry.pid);
        let serial = if entry.serial.is_empty() {
            "(any)"
        } else {
            &entry.serial
        };
        let on_detach = if entry.on_detach.is_empty() {
            "(none)"
        } else {
            &entry.on_detach
        };
        println!(
            "{:<20} {:<10} {:<12} {:<30} {}",
            entry.name, vid_pid, serial, entry.on_attach, on_detach
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

    // Also remove udev rule if exists
    match udev_rule::remove_udev_rule(name) {
        Ok(()) => {
            println!("Removed udev rule for '{}'.", name);
            let _ = udev_rule::reload_udev();
        }
        Err(e) => eprintln!("Warning: {}", e),
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

    // Get log files to display
    let log_files: Vec<PathBuf> = if let Some(ref device_name) = name {
        let path = log_dir.join(format!("{}.log", device_name));
        if path.exists() {
            vec![path]
        } else {
            eprintln!("No log file for device '{}'", device_name);
            return;
        }
    } else {
        // List all log files
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

    // Display last N lines of each file
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

    // Follow mode
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

            // Show devices in registry but not in layout
            let layout_names: std::collections::HashSet<_> = layout.device_order.iter().collect();
            let missing: Vec<_> = device_names.iter().filter(|n| !layout_names.contains(n)).collect();
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
        MaskLayoutAction::Import { path } => {
            match MaskLayout::import(&path) {
                Ok(layout) => {
                    match layout.save() {
                        Ok(()) => {
                            println!("Imported layout from: {}", path.display());
                            println!("Saved to: {}", MaskLayout::config_path().display());
                        }
                        Err(e) => {
                            eprintln!("Error saving layout: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error importing layout: {}", e);
                    std::process::exit(1);
                }
            }
        }
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

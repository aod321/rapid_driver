use crate::registry::DeviceEntry;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Directory for rapid_driver udev rules
const UDEV_RULES_DIR: &str = "/etc/udev/rules.d";
const RULE_PREFIX: &str = "99-rapid-driver";

/// Generate udev rule content for a device
fn generate_rule(entry: &DeviceEntry) -> String {
    let mut conditions = vec![
        format!("ATTRS{{idVendor}}==\"{}\"", entry.vid),
        format!("ATTRS{{idProduct}}==\"{}\"", entry.pid),
    ];

    if !entry.serial.is_empty() {
        conditions.push(format!("ATTRS{{serial}}==\"{}\"", entry.serial));
    }

    // For tty devices (like serial ports)
    let tty_rule = format!(
        "SUBSYSTEM==\"tty\", {}, SYMLINK+=\"{}\"",
        conditions.join(", "),
        entry.name
    );

    // For video4linux devices (cameras)
    let v4l_rule = format!(
        "SUBSYSTEM==\"video4linux\", {}, ATTR{{index}}==\"0\", SYMLINK+=\"{}\"",
        conditions.join(", "),
        entry.name
    );

    format!(
        "# rapid_driver rule for '{}'\n{}\n{}\n",
        entry.name, tty_rule, v4l_rule
    )
}

/// Create udev rule file for a device entry
pub fn create_udev_rule(entry: &DeviceEntry) -> Result<PathBuf, String> {
    let rule_content = generate_rule(entry);
    let rule_filename = format!("{}-{}.rules", RULE_PREFIX, entry.name);
    let rule_path = PathBuf::from(UDEV_RULES_DIR).join(&rule_filename);

    // Try to write directly first (if running as root)
    if fs::write(&rule_path, &rule_content).is_ok() {
        return Ok(rule_path);
    }

    // Fall back to sudo tee
    let status = Command::new("sudo")
        .arg("tee")
        .arg(&rule_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(rule_content.as_bytes())?;
            }
            child.wait()
        })
        .map_err(|e| format!("failed to run sudo: {}", e))?;

    if status.success() {
        Ok(rule_path)
    } else {
        Err("sudo tee failed - check permissions".to_string())
    }
}

/// Reload udev rules and trigger
pub fn reload_udev() -> Result<(), String> {
    let reload = Command::new("sudo")
        .args(["udevadm", "control", "--reload-rules"])
        .status()
        .map_err(|e| format!("failed to reload rules: {}", e))?;

    if !reload.success() {
        return Err("udevadm control --reload-rules failed".to_string());
    }

    let trigger = Command::new("sudo")
        .args(["udevadm", "trigger"])
        .status()
        .map_err(|e| format!("failed to trigger: {}", e))?;

    if !trigger.success() {
        return Err("udevadm trigger failed".to_string());
    }

    Ok(())
}

/// Remove udev rule for a device
pub fn remove_udev_rule(name: &str) -> Result<(), String> {
    let rule_filename = format!("{}-{}.rules", RULE_PREFIX, name);
    let rule_path = PathBuf::from(UDEV_RULES_DIR).join(&rule_filename);

    if !rule_path.exists() {
        return Ok(()); // Nothing to remove
    }

    // Try direct removal first
    if fs::remove_file(&rule_path).is_ok() {
        return Ok(());
    }

    // Fall back to sudo rm
    let status = Command::new("sudo")
        .arg("rm")
        .arg(&rule_path)
        .status()
        .map_err(|e| format!("failed to run sudo: {}", e))?;

    if status.success() {
        Ok(())
    } else {
        Err("sudo rm failed - check permissions".to_string())
    }
}

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo run                # Run in monitor mode (default)
cargo test               # Run all tests
cargo test test_name     # Run specific test
```

## Project Overview

rapid_driver is a USB device hotplug monitor that executes commands when registered USB devices are attached/detached. It publishes device status as a bitmask to shared memory at 500Hz for real-time hardware integration.

## Architecture

### Module Structure

- **main.rs**: CLI entry point using clap. Subcommands: `monitor` (default), `register`, `list`, `unregister`, `logs`, `mask-layout`
- **registry.rs**: Device registry stored at `~/.config/rapid_driver/registry.toml`. Handles VID/PID/serial matching with two-pass lookup (exact serial first, then model-only)
- **tui/mod.rs**: ratatui-based TUI with udev event loop. Manages process lifecycle (spawn, terminate, auto-restart with backoff)
- **mask/**: Hardware mask publishing subsystem
  - **shm.rs**: Writes 32-byte `HardwareMask` struct to `/dev/shm/rapid_hardware_mask` at 500Hz
  - **layout.rs**: Maps device names to bit positions in mask, stored at `~/.config/rapid_driver/mask_layout.toml`
  - **debug.rs**: Writes JSON debug state to `/dev/shm/rapid_hardware_mask.json` at 1Hz
- **udev_rule.rs**: Creates/removes `/etc/udev/rules.d/99-rapid-driver-*.rules` for device symlinks

### Shared Memory Protocol

The `HardwareMask` struct (32 bytes, cache-line aligned):
```
Offset  Field         Size  Description
0       magic         4     0x52415044 ("RAPD")
4       version       1     Protocol version (1)
5       device_count  1     Number of devices
8       mask          8     Online status bitmask
16      timestamp_ns  8     Nanoseconds since epoch
24      sequence      8     Monotonic sequence number
```

Device is "online" (bit set) when both USB connected AND on_attach process running.

### Process Management

- Processes spawned in new process groups (`process_group(0)`) for clean termination
- Graceful shutdown: SIGTERM to process group, wait 5s, then SIGKILL
- Auto-restart on exit (if USB still connected): max 5 restarts per 60s window, 2s delay between restarts
- 2s cooldown after device removal prevents rapid re-trigger

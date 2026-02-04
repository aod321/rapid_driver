//! Shared memory mask publishing.
//!
//! Publishes a 32-byte structure to `/dev/shm/rapid_hardware_mask` containing
//! device online status as a bitmask.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Magic number "RAPD" (0x52415044) in little-endian
pub const MAGIC: u32 = 0x44504152;

/// Shared memory file path
pub const SHM_PATH: &str = "/dev/shm/rapid_hardware_mask";

/// Protocol version
pub const VERSION: u8 = 1;

/// Hardware mask structure (32 bytes, cache-line aligned).
///
/// Layout:
/// ```text
/// Offset  Size  Field         Description
/// ──────  ────  ───────────   ─────────────────────────
/// 0       4     magic         0x52415044 ("RAPD")
/// 4       1     version       Protocol version (1)
/// 5       1     device_count  Number of devices
/// 6       2     _padding      Alignment padding
/// 8       8     mask          Online status bitmask
/// 16      8     timestamp_ns  Nanosecond timestamp
/// 24      8     sequence      Monotonic sequence number
/// ```
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy)]
pub struct HardwareMask {
    pub magic: u32,
    pub version: u8,
    pub device_count: u8,
    pub _padding: [u8; 2],
    pub mask: u64,
    pub timestamp_ns: u64,
    pub sequence: u64,
}

impl HardwareMask {
    /// Create a new HardwareMask with default values.
    #[allow(dead_code)]
    pub fn new(device_count: u8) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            device_count,
            _padding: [0; 2],
            mask: 0,
            timestamp_ns: 0,
            sequence: 0,
        }
    }

    /// Convert to raw bytes for writing.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }
}

// Verify struct size at compile time
const _: () = assert!(std::mem::size_of::<HardwareMask>() == 32);

/// Publisher for writing mask data to shared memory.
pub struct MaskPublisher {
    file: File,
    sequence: u64,
}

impl MaskPublisher {
    /// Create a new MaskPublisher, opening the shared memory file.
    pub fn new() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(SHM_PATH)?;

        Ok(Self { file, sequence: 0 })
    }

    /// Create a new MaskPublisher with a custom path (for testing).
    #[allow(dead_code)]
    pub fn with_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(path)?;

        Ok(Self { file, sequence: 0 })
    }

    /// Publish a mask value with the given device count.
    ///
    /// This performs an atomic write to the shared memory file.
    pub fn publish(&mut self, mask: u64, device_count: u8) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);

        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let data = HardwareMask {
            magic: MAGIC,
            version: VERSION,
            device_count,
            _padding: [0; 2],
            mask,
            timestamp_ns,
            sequence: self.sequence,
        };

        // Seek to beginning and write atomically
        use std::io::Seek;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(data.as_bytes())?;
        // Flush to ensure visibility
        self.file.flush()?;

        Ok(())
    }

    /// Get the current sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Remove the shared memory file.
    pub fn cleanup() {
        let _ = std::fs::remove_file(SHM_PATH);
    }
}

impl Drop for MaskPublisher {
    fn drop(&mut self) {
        // Don't remove the file on drop - let it persist for readers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_struct_size() {
        assert_eq!(std::mem::size_of::<HardwareMask>(), 32);
    }

    #[test]
    fn test_mask_alignment() {
        assert_eq!(std::mem::align_of::<HardwareMask>(), 32);
    }

    #[test]
    fn test_magic_value() {
        let mask = HardwareMask::new(5);
        let bytes = mask.as_bytes();
        // "RAPD" in little-endian is 0x44, 0x50, 0x41, 0x52
        assert_eq!(bytes[0], 0x52); // 'R'
        assert_eq!(bytes[1], 0x41); // 'A'
        assert_eq!(bytes[2], 0x50); // 'P'
        assert_eq!(bytes[3], 0x44); // 'D'
    }
}

//! Hardware mask generation and publishing module.
//!
//! Publishes device online status via shared memory at 500Hz.

pub mod debug;
mod layout;
mod shm;

pub use debug::DebugPublisher;
pub use layout::MaskLayout;
pub use shm::MaskPublisher;

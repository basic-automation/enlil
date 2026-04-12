//! Enlil Bridge — Inter-guest communication subsystem
//!
//! Provides clipboard sharing, drag-and-drop, shared filesystem,
//! and notification routing between guest VMs via `VirtIO` queues.

pub mod clipboard;
pub mod dragdrop;
pub mod notification;
pub mod shared_fs;
pub mod transport;

//! Intel High Definition Audio (HDA) controller emulation
//!
//! Minimal HDA controller that satisfies Windows driver detection.
//! Presents as a standard Intel HDA device on PCI bus. Windows will
//! load the inbox HD Audio driver (hdaudio.sys).

pub mod codec;
pub mod controller;

pub use codec::HdaCodec;
pub use controller::HdaController;

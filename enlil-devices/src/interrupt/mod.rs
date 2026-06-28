//! Interrupt virtualization — LAPIC, IOAPIC, MSI/MSI-X.
//!
//! Provides full virtual interrupt controller hierarchy:
//! ```text
//!  Device (IRQ) --> IOAPIC (24 RTEs) --+--> LAPIC 0 --> vCPU 0
//!                                      |
//!  MSI/MSI-X (addr+data) -------------+--> LAPIC 1 --> vCPU 1
//! ```

mod controller;
mod ioapic;
mod lapic;
mod line;
mod msi;
mod pic;
mod pirq;

/// Interrupt delivery mode (shared across LAPIC, IOAPIC, MSI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Fixed = 0,
    LowestPriority = 1,
    Smi = 2,
    Nmi = 4,
    Init = 5,
    StartUp = 6,
    ExtInt = 7,
}

impl DeliveryMode {
    /// Convert bits to delivery mode.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 0x7 {
            1 => Self::LowestPriority,
            2 => Self::Smi,
            4 => Self::Nmi,
            5 => Self::Init,
            6 => Self::StartUp,
            7 => Self::ExtInt,
            _ => Self::Fixed,
        }
    }
}

/// Trigger mode for interrupts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    Edge,
    Level,
}

/// An interrupt entry to be delivered to a LAPIC.
#[derive(Debug, Clone, Copy)]
pub struct InterruptEntry {
    pub vector: u8,
    pub delivery_mode: DeliveryMode,
    pub trigger_mode: TriggerMode,
    pub level: bool,
}

pub use controller::InterruptController;
pub use ioapic::IOAPIC_BASE;
pub use ioapic::{IoApic, RedirectionEntry};
pub use lapic::{IA32_TSC_DEADLINE, LAPIC_BASE, LAPIC_SVR, LocalApic};
pub use line::{IoApicMmio, LapicMmio, SharedInterruptController, SharedLapicMmio, isa_to_gsi};
pub use msi::MsiMessage;
pub use pic::{
    DualPic, ELCR_MASTER, ELCR_SLAVE, ElcrPort, MASTER_CMD, MASTER_DATA, Pic8259, PicMasterPort,
    PicSlavePort, SLAVE_CMD, SLAVE_DATA, SharedPic,
};
pub use pirq::{PIRQ_DEFAULT_IRQS, PIRQ_GSI_BASE, PIRQ_LINES, PIRQ_ROUTE_BASE, PirqRouter};

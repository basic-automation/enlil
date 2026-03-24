//! VirtIO MMIO transport layer.
//!
//! Implements the VirtIO MMIO transport specification for device discovery
//! and configuration by guest drivers.

use super::{VirtioDeviceType, VirtioStatus, VIRTIO_MMIO_MAGIC};

/// VirtIO MMIO register offsets.
pub mod regs {
    pub const MAGIC_VALUE: u64 = 0x000;
    pub const VERSION: u64 = 0x004;
    pub const DEVICE_ID: u64 = 0x008;
    pub const VENDOR_ID: u64 = 0x00c;
    pub const DEVICE_FEATURES: u64 = 0x010;
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    pub const DRIVER_FEATURES: u64 = 0x020;
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    pub const QUEUE_SEL: u64 = 0x030;
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    pub const QUEUE_NUM: u64 = 0x038;
    pub const QUEUE_READY: u64 = 0x044;
    pub const QUEUE_NOTIFY: u64 = 0x050;
    pub const INTERRUPT_STATUS: u64 = 0x060;
    pub const INTERRUPT_ACK: u64 = 0x064;
    pub const STATUS: u64 = 0x070;
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    pub const QUEUE_AVAIL_LOW: u64 = 0x090;
    pub const QUEUE_AVAIL_HIGH: u64 = 0x094;
    pub const QUEUE_USED_LOW: u64 = 0x0a0;
    pub const QUEUE_USED_HIGH: u64 = 0x0a4;
    pub const CONFIG_GENERATION: u64 = 0x0fc;
    pub const CONFIG_SPACE: u64 = 0x100;
}

/// Maximum number of virtqueues per device.
const MAX_QUEUES: usize = 8;
/// Default maximum queue size.
const DEFAULT_QUEUE_SIZE: u16 = 256;

/// Per-queue configuration state.
#[derive(Debug, Clone, Default)]
pub struct QueueConfig {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub desc_addr: u64,
    pub avail_addr: u64,
    pub used_addr: u64,
}

/// VirtIO MMIO transport state for a single device.
#[derive(Debug)]
pub struct MmioTransport {
    device_type: VirtioDeviceType,
    vendor_id: u32,
    device_features: u64,
    driver_features: u64,
    device_features_sel: u32,
    driver_features_sel: u32,
    status: VirtioStatus,
    interrupt_status: u32,
    queue_sel: u32,
    queues: Vec<QueueConfig>,
    config_generation: u32,
}

impl MmioTransport {
    /// Create a new MMIO transport for the given device type.
    pub fn new(device_type: VirtioDeviceType, vendor_id: u32, device_features: u64) -> Self {
        let mut queues = Vec::with_capacity(MAX_QUEUES);
        for _ in 0..MAX_QUEUES {
            queues.push(QueueConfig {
                max_size: DEFAULT_QUEUE_SIZE,
                ..Default::default()
            });
        }
        Self {
            device_type,
            vendor_id,
            device_features,
            driver_features: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            status: VirtioStatus::empty(),
            interrupt_status: 0,
            queue_sel: 0,
            queues,
            config_generation: 0,
        }
    }

    /// Handle an MMIO read at the given offset.
    pub fn read(&self, offset: u64, size: u8) -> u64 {
        if size != 4 && offset < regs::CONFIG_SPACE {
            return 0; // MMIO registers are 32-bit
        }
        match offset {
            regs::MAGIC_VALUE => VIRTIO_MMIO_MAGIC as u64,
            regs::VERSION => 2, // VirtIO modern
            regs::DEVICE_ID => self.device_type as u64,
            regs::VENDOR_ID => self.vendor_id as u64,
            regs::DEVICE_FEATURES => {
                if self.device_features_sel == 0 {
                    (self.device_features & 0xFFFF_FFFF) as u64
                } else {
                    (self.device_features >> 32) as u64
                }
            }
            regs::QUEUE_NUM_MAX => {
                self.current_queue().map(|q| q.max_size as u64).unwrap_or(0)
            }
            regs::QUEUE_READY => {
                self.current_queue().map(|q| q.ready as u64).unwrap_or(0)
            }
            regs::INTERRUPT_STATUS => self.interrupt_status as u64,
            regs::STATUS => self.status.bits() as u64,
            regs::CONFIG_GENERATION => self.config_generation as u64,
            _ => 0,
        }
    }

    /// Handle an MMIO write at the given offset.
    pub fn write(&mut self, offset: u64, _size: u8, value: u64) {
        let val32 = value as u32;
        match offset {
            regs::DEVICE_FEATURES_SEL => self.device_features_sel = val32,
            regs::DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features = (self.driver_features & !0xFFFF_FFFF) | (val32 as u64);
                } else {
                    self.driver_features = (self.driver_features & 0xFFFF_FFFF) | ((val32 as u64) << 32);
                }
            }
            regs::DRIVER_FEATURES_SEL => self.driver_features_sel = val32,
            regs::QUEUE_SEL => self.queue_sel = val32,
            regs::QUEUE_NUM => {
                if let Some(q) = self.current_queue_mut() {
                    q.size = value as u16;
                }
            }
            regs::QUEUE_READY => {
                if let Some(q) = self.current_queue_mut() {
                    q.ready = val32 != 0;
                }
            }
            regs::INTERRUPT_ACK => {
                self.interrupt_status &= !val32;
            }
            regs::STATUS => {
                self.status = VirtioStatus::from_bits_truncate(val32 as u8);
                if self.status.is_empty() {
                    self.reset();
                }
            }
            regs::QUEUE_DESC_LOW => {
                if let Some(q) = self.current_queue_mut() {
                    q.desc_addr = (q.desc_addr & !0xFFFF_FFFF) | (val32 as u64);
                }
            }
            regs::QUEUE_DESC_HIGH => {
                if let Some(q) = self.current_queue_mut() {
                    q.desc_addr = (q.desc_addr & 0xFFFF_FFFF) | ((val32 as u64) << 32);
                }
            }
            regs::QUEUE_AVAIL_LOW => {
                if let Some(q) = self.current_queue_mut() {
                    q.avail_addr = (q.avail_addr & !0xFFFF_FFFF) | (val32 as u64);
                }
            }
            regs::QUEUE_AVAIL_HIGH => {
                if let Some(q) = self.current_queue_mut() {
                    q.avail_addr = (q.avail_addr & 0xFFFF_FFFF) | ((val32 as u64) << 32);
                }
            }
            regs::QUEUE_USED_LOW => {
                if let Some(q) = self.current_queue_mut() {
                    q.used_addr = (q.used_addr & !0xFFFF_FFFF) | (val32 as u64);
                }
            }
            regs::QUEUE_USED_HIGH => {
                if let Some(q) = self.current_queue_mut() {
                    q.used_addr = (q.used_addr & 0xFFFF_FFFF) | ((val32 as u64) << 32);
                }
            }
            _ => {}
        }
    }

    /// Raise an interrupt (used queue notification).
    pub fn raise_used_interrupt(&mut self) {
        self.interrupt_status |= 1;
    }

    /// Raise a config change interrupt.
    pub fn raise_config_interrupt(&mut self) {
        self.interrupt_status |= 2;
        self.config_generation = self.config_generation.wrapping_add(1);
    }

    pub fn status(&self) -> VirtioStatus {
        self.status
    }

    pub fn driver_features(&self) -> u64 {
        self.driver_features
    }

    pub fn queue(&self, index: usize) -> Option<&QueueConfig> {
        self.queues.get(index)
    }

    fn current_queue(&self) -> Option<&QueueConfig> {
        self.queues.get(self.queue_sel as usize)
    }

    fn current_queue_mut(&mut self) -> Option<&mut QueueConfig> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    fn reset(&mut self) {
        self.driver_features = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.status = VirtioStatus::empty();
        self.interrupt_status = 0;
        self.queue_sel = 0;
        for q in &mut self.queues {
            *q = QueueConfig {
                max_size: DEFAULT_QUEUE_SIZE,
                ..Default::default()
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_transport() -> MmioTransport {
        MmioTransport::new(VirtioDeviceType::Block, 0x554D4551, 0x0000_0001_0000_0261)
    }

    #[test]
    fn magic_and_version() {
        let t = make_transport();
        assert_eq!(t.read(regs::MAGIC_VALUE, 4), VIRTIO_MMIO_MAGIC as u64);
        assert_eq!(t.read(regs::VERSION, 4), 2);
    }

    #[test]
    fn device_id_and_vendor() {
        let t = make_transport();
        assert_eq!(t.read(regs::DEVICE_ID, 4), VirtioDeviceType::Block as u64);
        assert_eq!(t.read(regs::VENDOR_ID, 4), 0x554D4551);
    }

    #[test]
    fn feature_negotiation() {
        let mut t = make_transport();
        // Read low features
        t.write(regs::DEVICE_FEATURES_SEL, 4, 0);
        let low = t.read(regs::DEVICE_FEATURES, 4);
        assert_eq!(low, 0x0000_0261);

        // Read high features
        t.write(regs::DEVICE_FEATURES_SEL, 4, 1);
        let high = t.read(regs::DEVICE_FEATURES, 4);
        assert_eq!(high, 0x0000_0001);

        // Write driver features
        t.write(regs::DRIVER_FEATURES_SEL, 4, 0);
        t.write(regs::DRIVER_FEATURES, 4, 0x0000_0201);
        t.write(regs::DRIVER_FEATURES_SEL, 4, 1);
        t.write(regs::DRIVER_FEATURES, 4, 0x0000_0001);
        assert_eq!(t.driver_features(), 0x0000_0001_0000_0201);
    }

    #[test]
    fn status_lifecycle() {
        let mut t = make_transport();
        assert_eq!(t.status(), VirtioStatus::empty());

        t.write(regs::STATUS, 4, VirtioStatus::ACKNOWLEDGE.bits() as u64);
        assert_eq!(t.status(), VirtioStatus::ACKNOWLEDGE);

        t.write(regs::STATUS, 4, (VirtioStatus::ACKNOWLEDGE | VirtioStatus::DRIVER).bits() as u64);
        assert!(t.status().contains(VirtioStatus::DRIVER));

        // Reset
        t.write(regs::STATUS, 4, 0);
        assert_eq!(t.status(), VirtioStatus::empty());
        assert_eq!(t.driver_features(), 0);
    }

    #[test]
    fn queue_configuration() {
        let mut t = make_transport();
        t.write(regs::QUEUE_SEL, 4, 0);
        assert_eq!(t.read(regs::QUEUE_NUM_MAX, 4), 256);

        t.write(regs::QUEUE_NUM, 4, 128);
        t.write(regs::QUEUE_DESC_LOW, 4, 0x1000);
        t.write(regs::QUEUE_DESC_HIGH, 4, 0);
        t.write(regs::QUEUE_AVAIL_LOW, 4, 0x2000);
        t.write(regs::QUEUE_AVAIL_HIGH, 4, 0);
        t.write(regs::QUEUE_USED_LOW, 4, 0x3000);
        t.write(regs::QUEUE_USED_HIGH, 4, 0);
        t.write(regs::QUEUE_READY, 4, 1);

        let q = t.queue(0).unwrap();
        assert_eq!(q.size, 128);
        assert_eq!(q.desc_addr, 0x1000);
        assert_eq!(q.avail_addr, 0x2000);
        assert_eq!(q.used_addr, 0x3000);
        assert!(q.ready);
    }

    #[test]
    fn interrupt_handling() {
        let mut t = make_transport();
        assert_eq!(t.read(regs::INTERRUPT_STATUS, 4), 0);

        t.raise_used_interrupt();
        assert_eq!(t.read(regs::INTERRUPT_STATUS, 4), 1);

        t.raise_config_interrupt();
        assert_eq!(t.read(regs::INTERRUPT_STATUS, 4), 3);

        // ACK used interrupt
        t.write(regs::INTERRUPT_ACK, 4, 1);
        assert_eq!(t.read(regs::INTERRUPT_STATUS, 4), 2);
    }
}

//! Minimal bare-metal management-network driver (Phase 6.5).
//!
//! The service VM (6.6) and the management console need a network path that
//! does not depend on a host OS: this module is the boot kernel's bare-metal
//! NIC driver. Deliberately minimal — one NIC, TX/RX, no network stack: an
//! Intel 8254x ("e1000") NIC programmed through its MMIO register file with
//! classic legacy TX/RX descriptor rings, plus PCI discovery that also
//! recognizes the e1000e and virtio-net-pci devices QEMU presents (their full
//! bring-up is a follow-on slice).
//!
//! The register/descriptor/frame math is pure and host-tested; the MMIO pokes
//! assemble only for the firmware target (`target_os = "uefi"`), so the
//! register programming never runs — or faults — on the hosted test build.

// ---------------------------------------------------------------------------
// PCI discovery: which NICs the driver knows.
// ---------------------------------------------------------------------------

/// PCI vendor id: Intel.
pub const PCI_VENDOR_INTEL: u16 = 0x8086;

/// PCI vendor id: Red Hat (virtio devices).
pub const PCI_VENDOR_VIRTIO: u16 = 0x1AF4;

/// Intel 82540EM ("e1000", the `-device e1000` QEMU NIC).
pub const E1000_DEV_82540EM: u16 = 0x100E;
/// Intel 82545EM (copper).
pub const E1000_DEV_82545EM_COPPER: u16 = 0x100F;
/// Intel 82546EB (copper).
pub const E1000_DEV_82546EB_COPPER: u16 = 0x1010;
/// Intel 82545EM (fiber).
pub const E1000_DEV_82545EM_FIBER: u16 = 0x1011;
/// Intel 82546EB (fiber).
pub const E1000_DEV_82546EB_FIBER: u16 = 0x1012;
/// Intel 82541EI.
pub const E1000_DEV_82541EI: u16 = 0x1013;
/// Intel 82540EP.
pub const E1000_DEV_82540EP: u16 = 0x1017;
/// Intel 82541EI (mobile).
pub const E1000_DEV_82541EI_MOBILE: u16 = 0x1018;
/// Intel 82547EI.
pub const E1000_DEV_82547EI: u16 = 0x101A;
/// Intel 82545GM (copper).
pub const E1000_DEV_82545GM_COPPER: u16 = 0x1026;
/// Intel 82547GI.
pub const E1000_DEV_82547GI: u16 = 0x1075;
/// Intel 82541GI.
pub const E1000_DEV_82541GI: u16 = 0x1076;
/// Intel 82541ER.
pub const E1000_DEV_82541ER: u16 = 0x1078;
/// Intel 82546GB (copper).
pub const E1000_DEV_82546GB_COPPER: u16 = 0x1079;
/// Intel 82544EI (copper).
pub const E1000_DEV_82544EI_COPPER: u16 = 0x1107;

/// Intel 82574L ("e1000e", the default NIC on QEMU's q35 machine type).
pub const E1000E_DEV_82574L: u16 = 0x10D3;
/// Intel 82576 ("e1000e").
pub const E1000E_DEV_82576: u16 = 0x10C9;
/// Intel 82579LM ("e1000e").
pub const E1000E_DEV_82579LM: u16 = 0x1502;

/// virtio-net-pci, legacy/transitional PCI id.
pub const VIRTIO_NET_DEV_LEGACY: u16 = 0x1000;
/// virtio-net-pci, virtio-1.0+ PCI id (base 0x1040 + network device 1).
pub const VIRTIO_NET_DEV_MODERN: u16 = 0x1041;

/// Whether `vendor_id`/`device_id` is an Intel 8254x ("e1000") NIC — the
/// device this module's TX/RX engine drives.
#[must_use]
pub const fn is_e1000_device(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == PCI_VENDOR_INTEL
        && matches!(
            device_id,
            E1000_DEV_82540EM
                | E1000_DEV_82545EM_COPPER
                | E1000_DEV_82546EB_COPPER
                | E1000_DEV_82545EM_FIBER
                | E1000_DEV_82546EB_FIBER
                | E1000_DEV_82541EI
                | E1000_DEV_82540EP
                | E1000_DEV_82541EI_MOBILE
                | E1000_DEV_82547EI
                | E1000_DEV_82545GM_COPPER
                | E1000_DEV_82547GI
                | E1000_DEV_82541GI
                | E1000_DEV_82541ER
                | E1000_DEV_82546GB_COPPER
                | E1000_DEV_82544EI_COPPER
        )
}

/// Whether `vendor_id`/`device_id` is an Intel 8257x/82574 ("e1000e") NIC.
///
/// Recognized for discovery; the e1000e shares the legacy descriptor format
/// but its full init sequence is a follow-on slice.
#[must_use]
pub const fn is_e1000e_device(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == PCI_VENDOR_INTEL
        && matches!(
            device_id,
            E1000E_DEV_82574L | E1000E_DEV_82576 | E1000E_DEV_82579LM
        )
}

/// Whether `vendor_id`/`device_id` is a virtio-net-pci NIC.
///
/// Recognized for discovery; programming its virtqueues through the virtio
/// PCI capabilities is a follow-on slice.
#[must_use]
pub const fn is_virtio_net_device(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == PCI_VENDOR_VIRTIO
        && matches!(device_id, VIRTIO_NET_DEV_LEGACY | VIRTIO_NET_DEV_MODERN)
}

/// What kind of management NIC PCI discovery found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NicKind {
    /// Intel 8254x — the full TX/RX engine in this module drives it.
    E1000 {
        /// The PCI device id (which 8254x member it is).
        device_id: u16,
    },
    /// Intel 8257x/82574 ("e1000e") — recognized; full init is a later slice.
    E1000e {
        /// The PCI device id.
        device_id: u16,
    },
    /// virtio-net-pci — recognized; virtqueue setup is a later slice.
    VirtioNet,
    /// Some other network-class controller the driver does not know.
    UnknownNetwork,
}

impl NicKind {
    /// Short human-readable name for boot logs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::E1000 { .. } => "e1000",
            Self::E1000e { .. } => "e1000e",
            Self::VirtioNet => "virtio-net-pci",
            Self::UnknownNetwork => "unknown-network",
        }
    }
}

/// Classify a PCI function as a management NIC, if it is one.
///
/// Device-id matches are authoritative (an 82540EM is an e1000 whatever its
/// class code claims); a network-class function with an unknown id still
/// reports as [`NicKind::UnknownNetwork`], and anything else is not a NIC.
#[must_use]
pub const fn nic_kind_of(vendor_id: u16, device_id: u16, class: u8) -> Option<NicKind> {
    if is_e1000_device(vendor_id, device_id) {
        return Some(NicKind::E1000 { device_id });
    }
    if is_e1000e_device(vendor_id, device_id) {
        return Some(NicKind::E1000e { device_id });
    }
    if is_virtio_net_device(vendor_id, device_id) {
        return Some(NicKind::VirtioNet);
    }
    if class == crate::pci::CLASS_NETWORK {
        return Some(NicKind::UnknownNetwork);
    }
    None
}

// ---------------------------------------------------------------------------
// e1000 (8254x) register file.
// ---------------------------------------------------------------------------

/// Device Control.
pub const E1000_CTRL: u32 = 0x0000;
/// CTRL bit: device reset (self-clearing).
pub const E1000_CTRL_RST: u32 = 1 << 26;

/// Interrupt Cause Read (reading clears the asserted causes).
pub const E1000_ICR: u32 = 0x00C0;
/// Interrupt Mask Set.
pub const E1000_IMS: u32 = 0x00D0;
/// Interrupt Mask Clear.
pub const E1000_IMC: u32 = 0x00D8;

/// Receive Control.
pub const E1000_RCTL: u32 = 0x0100;
/// RCTL bit: receiver enable.
pub const E1000_RCTL_EN: u32 = 1 << 1;
/// RCTL bit: broadcast accept mode.
pub const E1000_RCTL_BAM: u32 = 1 << 15;
/// RCTL bit: strip the Ethernet CRC (so `rx_len` is the frame length).
pub const E1000_RCTL_SECRC: u32 = 1 << 26;
/// RCTL buffer-size field value for 2048-byte buffers (bits 17:16 = 00b).
pub const E1000_RCTL_BSIZE_2048: u32 = 0b00 << 16;

/// RCTL programming for the minimal RX path: enabled, broadcast accepted,
/// 2048-byte buffers, CRC stripped.
pub const RCTL_INIT: u32 = E1000_RCTL_EN | E1000_RCTL_BAM | E1000_RCTL_SECRC;

/// Transmit Control.
pub const E1000_TCTL: u32 = 0x0400;
/// TCTL bit: transmitter enable.
pub const E1000_TCTL_EN: u32 = 1 << 1;
/// TCTL bit: pad short packets to the 64-byte minimum.
pub const E1000_TCTL_PSP: u32 = 1 << 3;

/// TCTL programming for the minimal TX path: enabled, short packets padded.
pub const TCTL_INIT: u32 = E1000_TCTL_EN | E1000_TCTL_PSP;

/// Transmit Inter-Packet Gap (datasheet §13.4.34 full-duplex recommendation:
/// IPGT=8, IPGR1=8, IPGR2=6).
pub const E1000_TIPG: u32 = 0x0410;
/// TIPG value for the recommendation above.
pub const E1000_TIPG_FULL_DUPLEX: u32 = 8 | (8 << 10) | (6 << 20);

/// RX descriptor base address low/high.
pub const E1000_RDBAL: u32 = 0x2800;
/// RX descriptor base address high.
pub const E1000_RDBAH: u32 = 0x2804;
/// RX descriptor ring length in bytes (must be a multiple of 128).
pub const E1000_RDLEN: u32 = 0x2808;
/// RX descriptor head (next descriptor the hardware will fill).
pub const E1000_RDH: u32 = 0x2810;
/// RX descriptor tail (last descriptor the hardware may use — inclusive).
pub const E1000_RDT: u32 = 0x2818;
/// RX interrupt delay timer (0 = no delay; the driver polls anyway).
pub const E1000_RDTR: u32 = 0x2820;

/// TX descriptor base address low.
pub const E1000_TDBAL: u32 = 0x3800;
/// TX descriptor base address high.
pub const E1000_TDBAH: u32 = 0x3804;
/// TX descriptor ring length in bytes (must be a multiple of 128).
pub const E1000_TDLEN: u32 = 0x3808;
/// TX descriptor head (next descriptor the hardware will send).
pub const E1000_TDH: u32 = 0x3810;
/// TX descriptor tail (next descriptor software will write — exclusive).
pub const E1000_TDT: u32 = 0x3818;
/// TX interrupt delay value (0 = no delay; the driver polls anyway).
pub const E1000_TIDV: u32 = 0x3820;

/// Multicast Table Array: 128 dwords the driver clears (multicast filtering
/// off; broadcast is accepted via RCTL.BAM).
pub const E1000_MTA: u32 = 0x5200;
/// Entries in the Multicast Table Array.
pub const E1000_MTA_ENTRIES: u32 = 128;

/// Receive Address 0 low (bytes 0–3 of the station MAC, little-endian).
pub const E1000_RAL0: u32 = 0x5400;
/// Receive Address 0 high (bytes 4–5 + the Address-Valid bit).
pub const E1000_RAH0: u32 = 0x5404;
/// RAH bit: the receive address is valid.
pub const E1000_RAH_AV: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// Legacy descriptors.
// ---------------------------------------------------------------------------

/// A legacy e1000 transmit descriptor: 16 bytes.
pub const DESC_SIZE: usize = 16;

/// Suggested descriptor-ring size: 32 entries × 16 B = 512 B, a multiple of
/// the 128-byte ring-length granularity the spec requires.
pub const RING_ENTRIES: usize = 32;

/// The RX packet-buffer size the driver arms (matches RCTL 2048-byte buffers).
pub const RX_BUFFER_SIZE: usize = 2048;

/// TX descriptor command: end of packet.
pub const TX_CMD_EOP: u8 = 0x01;
/// TX descriptor command: insert FCS/CRC.
pub const TX_CMD_IFCS: u8 = 0x02;
/// TX descriptor command: report status (write back DD when done).
pub const TX_CMD_RS: u8 = 0x08;
/// TX descriptor status: descriptor done.
pub const TX_STATUS_DD: u8 = 0x01;

/// RX descriptor status: descriptor done (a frame was written).
pub const RX_STATUS_DD: u8 = 0x01;
/// RX descriptor status: end of packet.
pub const RX_STATUS_EOP: u8 = 0x02;

/// A legacy e1000 transmit descriptor (16 bytes, 16-byte aligned in the ring).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TxDesc {
    /// Physical address of the frame buffer.
    pub addr: u64,
    /// Frame length in bytes.
    pub length: u16,
    /// Checksum offset (unused — no offload).
    pub cso: u8,
    /// Command byte ([`TX_CMD_EOP`] | [`TX_CMD_IFCS`] | [`TX_CMD_RS`]).
    pub cmd: u8,
    /// Status byte written back by the hardware ([`TX_STATUS_DD`]).
    pub status: u8,
    /// Checksum start (unused — no offload).
    pub css: u8,
    /// Special (unused).
    pub special: u16,
}

/// A legacy e1000 receive descriptor (16 bytes, 16-byte aligned in the ring).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RxDesc {
    /// Physical address of the 2048-byte packet buffer.
    pub addr: u64,
    /// Received frame length — valid once the status says done.
    pub length: u16,
    /// Packet checksum (unused — no offload).
    pub checksum: u16,
    /// Status byte written by the hardware ([`RX_STATUS_DD`]).
    pub status: u8,
    /// Error flags (unused by the minimal path).
    pub errors: u8,
    /// Special (unused).
    pub special: u16,
}

/// Build a transmit descriptor for `len` bytes at physical `addr`.
///
/// The command requests end-of-packet, hardware FCS insertion, and a
/// status write-back so the driver can poll for completion.
#[must_use]
pub const fn tx_desc(addr: u64, len: u16) -> TxDesc {
    TxDesc {
        addr,
        length: len,
        cso: 0,
        cmd: TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS,
        status: 0,
        css: 0,
        special: 0,
    }
}

/// Whether the hardware has finished with this TX descriptor (DD set) — the
/// buffer may be reused once this is true.
#[must_use]
pub const fn tx_done(desc: &TxDesc) -> bool {
    desc.status & TX_STATUS_DD != 0
}

/// Build an empty receive descriptor pointing at the buffer at `addr`,
/// owned by the hardware.
#[must_use]
pub const fn rx_desc(addr: u64) -> RxDesc {
    RxDesc {
        addr,
        length: 0,
        checksum: 0,
        status: 0,
        errors: 0,
        special: 0,
    }
}

/// Whether the hardware has written a received frame into this descriptor.
#[must_use]
pub const fn rx_ready(desc: &RxDesc) -> bool {
    desc.status & RX_STATUS_DD != 0
}

/// The received frame's length — valid once [`rx_ready`] is true.
#[must_use]
pub const fn rx_len(desc: &RxDesc) -> u16 {
    desc.length
}

/// The next descriptor-ring index with wraparound.
#[must_use]
pub const fn next_index(idx: usize, len: usize) -> usize {
    let next = idx + 1;
    if next >= len { 0 } else { next }
}

// ---------------------------------------------------------------------------
// Station MAC.
// ---------------------------------------------------------------------------

/// Decode the 6-byte station MAC from the RAL/RAH receive-address registers.
#[must_use]
pub const fn mac_of_rar(ral: u32, rah: u32) -> [u8; 6] {
    [
        (ral & 0xFF) as u8,
        ((ral >> 8) & 0xFF) as u8,
        ((ral >> 16) & 0xFF) as u8,
        ((ral >> 24) & 0xFF) as u8,
        (rah & 0xFF) as u8,
        ((rah >> 8) & 0xFF) as u8,
    ]
}

/// The RAL value programming `mac` (bytes 0–3, little-endian).
#[must_use]
pub const fn ral_of_mac(mac: &[u8; 6]) -> u32 {
    (mac[0] as u32) | ((mac[1] as u32) << 8) | ((mac[2] as u32) << 16) | ((mac[3] as u32) << 24)
}

/// The RAH value programming `mac` (bytes 4–5) with the Address-Valid bit.
#[must_use]
pub const fn rah_of_mac(mac: &[u8; 6]) -> u32 {
    E1000_RAH_AV | (mac[4] as u32) | ((mac[5] as u32) << 8)
}

/// Format a MAC as `52:54:00:12:34:56` into `buf` (17 bytes used).
pub fn format_mac<'a>(mac: &[u8; 6], buf: &'a mut [u8; 18]) -> &'a str {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut cursor = 0;
    for (i, byte) in mac.iter().enumerate() {
        if i != 0 {
            buf[cursor] = b':';
            cursor += 1;
        }
        buf[cursor] = DIGITS[usize::from(byte >> 4)];
        buf[cursor + 1] = DIGITS[usize::from(byte & 0x0F)];
        cursor += 2;
    }
    // The bytes just written are ASCII, so this cannot fail.
    core::str::from_utf8(&buf[..cursor]).unwrap_or("?")
}

// ---------------------------------------------------------------------------
// Ethernet frame fields (what the management path inspects).
// ---------------------------------------------------------------------------

/// Ethernet header length in bytes.
pub const ETH_HEADER_LEN: usize = 14;

/// Destination MAC of an Ethernet frame (`None` for a runt frame).
#[must_use]
pub fn eth_dst(frame: &[u8]) -> Option<[u8; 6]> {
    frame
        .get(..6)
        .and_then(|bytes| <[u8; 6]>::try_from(bytes).ok())
}

/// Source MAC of an Ethernet frame (`None` for a runt frame).
#[must_use]
pub fn eth_src(frame: &[u8]) -> Option<[u8; 6]> {
    frame
        .get(6..12)
        .and_then(|bytes| <[u8; 6]>::try_from(bytes).ok())
}

/// `EtherType` of an Ethernet frame, big-endian (`None` for a runt frame).
#[must_use]
pub fn ethertype(frame: &[u8]) -> Option<u16> {
    frame
        .get(12..14)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Whether `mac` is the broadcast address.
#[must_use]
pub const fn is_broadcast_mac(mac: &[u8; 6]) -> bool {
    mac[0] == 0xFF
        && mac[1] == 0xFF
        && mac[2] == 0xFF
        && mac[3] == 0xFF
        && mac[4] == 0xFF
        && mac[5] == 0xFF
}

// ---------------------------------------------------------------------------
// Firmware target: the live driver.
// ---------------------------------------------------------------------------

#[cfg(target_os = "uefi")]
pub use hw::{E1000, read_mac};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        E1000_CTRL, E1000_CTRL_RST, E1000_ICR, E1000_IMC, E1000_MTA, E1000_MTA_ENTRIES, E1000_RAH0,
        E1000_RAL0, E1000_RCTL, E1000_RDBAH, E1000_RDBAL, E1000_RDH, E1000_RDLEN, E1000_RDT,
        E1000_RDTR, E1000_TCTL, E1000_TDBAH, E1000_TDBAL, E1000_TDH, E1000_TDLEN, E1000_TDT,
        E1000_TIDV, E1000_TIPG, E1000_TIPG_FULL_DUPLEX, RCTL_INIT, RX_BUFFER_SIZE, RxDesc,
        TCTL_INIT, TX_STATUS_DD, TxDesc, mac_of_rar, next_index, rah_of_mac, ral_of_mac, rx_desc,
        rx_len, rx_ready, tx_desc, tx_done,
    };

    /// Volatile MMIO u32 read.
    ///
    /// # Safety
    ///
    /// `base + offset` must be a mapped MMIO register of the NIC.
    unsafe fn mmio_read(base: u64, offset: u32) -> u32 {
        // SAFETY: the caller guarantees a mapped register.
        unsafe { core::ptr::read_volatile((base + u64::from(offset)) as *const u32) }
    }

    /// Volatile MMIO u32 write.
    ///
    /// # Safety
    ///
    /// `base + offset` must be a mapped MMIO register of the NIC.
    unsafe fn mmio_write(base: u64, offset: u32, value: u32) {
        // SAFETY: the caller guarantees a mapped register.
        unsafe { core::ptr::write_volatile((base + u64::from(offset)) as *mut u32, value) }
    }

    /// Read the NIC's station MAC straight from RAR[0] — the discovery-time
    /// proof the MMIO window is live, before any ring is programmed.
    ///
    /// # Safety
    ///
    /// `mmio_base` must be the NIC's BAR0 register window (mapped).
    #[must_use]
    pub unsafe fn read_mac(mmio_base: u64) -> [u8; 6] {
        // SAFETY: the caller guarantees the mapped register window.
        let ral = unsafe { mmio_read(mmio_base, E1000_RAL0) };
        let rah = unsafe { mmio_read(mmio_base, E1000_RAH0) };
        mac_of_rar(ral, rah)
    }

    /// An Intel 8254x ("e1000") NIC bound to its MMIO register file.
    ///
    /// The descriptor rings and packet buffers are caller-owned: the service
    /// VM allocates them from DMA-able (identity-mapped) memory and hands
    /// them to [`E1000::init`]; they must outlive the device.
    pub struct E1000 {
        /// MMIO base of the register file (BAR0).
        base: u64,
        /// Virtual address of the TX descriptor ring (set by `init`).
        tx_ring: *mut TxDesc,
        /// Virtual address of the RX descriptor ring (set by `init`).
        rx_ring: *mut RxDesc,
        /// Descriptors per ring.
        entries: usize,
        /// Next TX descriptor software will write.
        tx_tail: usize,
        /// Next RX descriptor the hardware will complete.
        rx_head: usize,
    }

    impl E1000 {
        /// Bind to the MMIO register file at `mmio_base`.
        ///
        /// # Safety
        ///
        /// `mmio_base` must be the NIC's programmed BAR0 and the register
        /// window must be mapped.
        pub unsafe fn new(mmio_base: u64) -> Self {
            Self {
                base: mmio_base,
                tx_ring: core::ptr::null_mut(),
                rx_ring: core::ptr::null_mut(),
                entries: 0,
                tx_tail: 0,
                rx_head: 0,
            }
        }

        /// MMIO register read.
        fn read(&self, offset: u32) -> u32 {
            // SAFETY: the constructor guarantees the mapped register window.
            unsafe { mmio_read(self.base, offset) }
        }

        /// MMIO register write.
        fn write(&self, offset: u32, value: u32) {
            // SAFETY: the constructor guarantees the mapped register window.
            unsafe { mmio_write(self.base, offset, value) }
        }

        /// Reset the device and wait (bounded) for the reset bit to clear.
        ///
        /// Returns `false` if the bit never clears — a wedged device must not
        /// hang boot.
        pub fn reset(&self) -> bool {
            self.write(E1000_CTRL, E1000_CTRL_RST);
            let mut tries = 0u32;
            while self.read(E1000_CTRL) & E1000_CTRL_RST != 0 {
                tries += 1;
                if tries > 10_000 {
                    return false;
                }
                core::hint::spin_loop();
            }
            // Drain any interrupt causes the reset left asserted.
            let _ = self.read(E1000_ICR);
            true
        }

        /// Program the descriptor rings and bring the MAC up.
        ///
        /// Resets the device, masks every interrupt (the management path
        /// polls — no IDT vector is wired), clears the multicast table,
        /// programs the station MAC into RAR[0], marks every TX descriptor
        /// free, pre-arms every RX descriptor with its 2048-byte buffer from
        /// the contiguous `rx_buffers_phys` region, and enables the receiver
        /// and transmitter.
        ///
        /// `entries` must be 8–4096 and a multiple of 8 (the ring length is a
        /// multiple of 128 bytes, per the spec).
        ///
        /// # Safety
        ///
        /// The rings must be live, exclusively owned, 16-byte-aligned
        /// `entries`-element arrays that outlive the device, and — because
        /// the kernel runs identity-mapped — their virtual addresses are the
        /// DMA addresses the NIC is programmed with. `rx_buffers_phys` must
        /// be `entries` contiguous 2048-byte DMA-able buffers.
        pub unsafe fn init(
            &mut self,
            tx_ring: *mut TxDesc,
            rx_ring: *mut RxDesc,
            rx_buffers_phys: u64,
            entries: usize,
            mac: &[u8; 6],
        ) -> bool {
            let Ok(entries32) = u32::try_from(entries) else {
                return false;
            };
            if !(8..=4096).contains(&entries) || !entries.is_multiple_of(8) {
                return false;
            }
            if tx_ring.is_null() || rx_ring.is_null() || rx_buffers_phys == 0 {
                return false;
            }
            if !self.reset() {
                return false;
            }
            // Mask every interrupt — the management path polls.
            self.write(E1000_IMC, u32::MAX);
            // No multicast filtering; broadcast arrives via RCTL.BAM.
            for i in 0..E1000_MTA_ENTRIES {
                self.write(E1000_MTA + i * 4, 0);
            }
            self.write(E1000_TIPG, E1000_TIPG_FULL_DUPLEX);
            // Program the station MAC into RAR[0].
            self.write(E1000_RAL0, ral_of_mac(mac));
            self.write(E1000_RAH0, rah_of_mac(mac));

            // The kernel runs identity-mapped, so the rings' virtual addresses
            // are the DMA addresses the NIC is programmed with.
            let tx_phys = tx_ring as u64;
            let rx_phys = rx_ring as u64;
            // RDLEN/TDLEN are byte counts; 16 bytes per descriptor.
            let ring_bytes = entries32 * 16;

            // Mark every TX descriptor free and pre-arm every RX descriptor
            // with its buffer, handing the whole RX ring to the hardware.
            for i in 0..entries {
                // SAFETY: the caller guarantees live, exclusively-owned rings.
                unsafe {
                    let mut tx = TxDesc::default();
                    tx.status = TX_STATUS_DD;
                    tx_ring.add(i).write(tx);
                    rx_ring.add(i).write(rx_desc(
                        rx_buffers_phys + (i as u64) * RX_BUFFER_SIZE as u64,
                    ));
                }
            }

            // RX ring: every descriptor is hardware-owned, so the tail is the
            // *last* descriptor (inclusive tail semantics).
            self.write(E1000_RDBAL, (rx_phys & 0xFFFF_FFFF) as u32);
            self.write(E1000_RDBAH, (rx_phys >> 32) as u32);
            self.write(E1000_RDLEN, ring_bytes);
            self.write(E1000_RDH, 0);
            self.write(E1000_RDT, entries32 - 1);
            self.write(E1000_RDTR, 0);
            self.write(E1000_RCTL, RCTL_INIT);
            // TX ring: empty — head and tail both start at descriptor 0
            // (exclusive tail: TDT is the next descriptor software writes).
            self.write(E1000_TDBAL, (tx_phys & 0xFFFF_FFFF) as u32);
            self.write(E1000_TDBAH, (tx_phys >> 32) as u32);
            self.write(E1000_TDLEN, ring_bytes);
            self.write(E1000_TDH, 0);
            self.write(E1000_TDT, 0);
            self.write(E1000_TIDV, 0);
            self.write(E1000_TCTL, TCTL_INIT);

            self.tx_ring = tx_ring;
            self.rx_ring = rx_ring;
            self.entries = entries;
            self.tx_tail = 0;
            self.rx_head = 0;
            true
        }

        /// Queue one frame for transmission and ring the TX doorbell.
        ///
        /// `frame_phys` is the frame's physical (= identity-mapped virtual)
        /// address; the buffer must stay live until the descriptor completes.
        /// Returns the in-flight descriptor index, or `None` when the ring is
        /// full. Poll [`E1000::tx_done_at`] before reusing the buffer.
        pub fn transmit(&mut self, frame_phys: u64, len: usize) -> Option<usize> {
            if self.entries == 0 {
                return None;
            }
            let tail = self.tx_tail;
            if !self.tx_done_at(tail) {
                return None; // ring full — the tail buffer is still in flight
            }
            let len16 = u16::try_from(len.min(usize::from(u16::MAX))).unwrap_or(u16::MAX);
            // SAFETY: `init` guarantees a live, exclusively-owned ring, and
            // the DD check above proves this descriptor is software-owned.
            unsafe {
                self.tx_ring.add(tail).write(tx_desc(frame_phys, len16));
            }
            let next = next_index(tail, self.entries);
            self.tx_tail = next;
            // TDT is the next descriptor software will write (exclusive).
            self.write(E1000_TDT, u32::try_from(next).unwrap_or(u32::MAX));
            Some(tail)
        }

        /// Whether the TX descriptor at `idx` has completed (DD set) — its
        /// buffer may be reused once this is true.
        #[must_use]
        pub fn tx_done_at(&self, idx: usize) -> bool {
            if self.entries == 0 || idx >= self.entries {
                return false;
            }
            // SAFETY: `init` guarantees a live, exclusively-owned ring.
            let desc = unsafe { self.tx_ring.add(idx).read() };
            tx_done(&desc)
        }

        /// Take the next completed RX descriptor, if the NIC has written a frame.
        ///
        /// Returns the frame's physical buffer address and length. The buffer
        /// is the one the descriptor was armed with; re-arm it with
        /// [`E1000::rx_rearm`] once the frame is consumed so the ring never
        /// runs dry.
        #[must_use]
        pub fn receive(&mut self) -> Option<(u64, u16)> {
            if self.entries == 0 {
                return None;
            }
            // SAFETY: `init` guarantees a live, exclusively-owned ring.
            let desc = unsafe { self.rx_ring.add(self.rx_head).read() };
            if !rx_ready(&desc) {
                return None;
            }
            let frame = (desc.addr, rx_len(&desc));
            self.rx_head = next_index(self.rx_head, self.entries);
            Some(frame)
        }

        /// Re-arm RX descriptor `idx` with the empty buffer at `buffer_phys`
        /// and hand it back to the hardware (RDT moves onto it — inclusive).
        pub fn rx_rearm(&self, idx: usize, buffer_phys: u64) {
            if self.entries == 0 || idx >= self.entries {
                return;
            }
            // SAFETY: `init` guarantees a live, exclusively-owned ring, and
            // the caller re-arms only descriptors `receive` returned.
            unsafe {
                self.rx_ring.add(idx).write(rx_desc(buffer_phys));
            }
            self.write(E1000_RDT, u32::try_from(idx).unwrap_or(u32::MAX));
        }

        /// The station MAC currently programmed in RAR[0].
        #[must_use]
        pub fn mac(&self) -> [u8; 6] {
            mac_of_rar(self.read(E1000_RAL0), self.read(E1000_RAH0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e1000_device_ids_classify() {
        assert!(is_e1000_device(PCI_VENDOR_INTEL, E1000_DEV_82540EM));
        assert!(is_e1000_device(PCI_VENDOR_INTEL, 0x100F));
        assert!(is_e1000_device(PCI_VENDOR_INTEL, 0x1107));
        // e1000e and virtio ids are not e1000.
        assert!(!is_e1000_device(PCI_VENDOR_INTEL, E1000E_DEV_82574L));
        assert!(!is_e1000_device(PCI_VENDOR_VIRTIO, VIRTIO_NET_DEV_LEGACY));
        // Right id, wrong vendor is not e1000.
        assert!(!is_e1000_device(0x1234, E1000_DEV_82540EM));
    }

    #[test]
    fn e1000e_and_virtio_net_classify() {
        assert!(is_e1000e_device(PCI_VENDOR_INTEL, 0x10D3));
        assert!(!is_e1000e_device(PCI_VENDOR_INTEL, E1000_DEV_82540EM));
        assert!(is_virtio_net_device(
            PCI_VENDOR_VIRTIO,
            VIRTIO_NET_DEV_LEGACY
        ));
        assert!(is_virtio_net_device(
            PCI_VENDOR_VIRTIO,
            VIRTIO_NET_DEV_MODERN
        ));
        assert!(!is_virtio_net_device(PCI_VENDOR_VIRTIO, 0x1042)); // virtio-blk
    }

    #[test]
    fn nic_kind_of_classifies() {
        assert_eq!(
            nic_kind_of(PCI_VENDOR_INTEL, 0x100E, crate::pci::CLASS_NETWORK),
            Some(NicKind::E1000 { device_id: 0x100E })
        );
        // Device-id match is authoritative even with a surprising class code.
        assert_eq!(
            nic_kind_of(PCI_VENDOR_INTEL, 0x100E, 0xFF),
            Some(NicKind::E1000 { device_id: 0x100E })
        );
        assert_eq!(
            nic_kind_of(PCI_VENDOR_INTEL, 0x10D3, crate::pci::CLASS_NETWORK),
            Some(NicKind::E1000e { device_id: 0x10D3 })
        );
        assert_eq!(
            nic_kind_of(PCI_VENDOR_VIRTIO, VIRTIO_NET_DEV_MODERN, 0x02),
            Some(NicKind::VirtioNet)
        );
        // An unknown network-class controller still reports as a NIC.
        assert_eq!(
            nic_kind_of(0x1234, 0x5678, crate::pci::CLASS_NETWORK),
            Some(NicKind::UnknownNetwork)
        );
        // Anything else is not a NIC.
        assert_eq!(nic_kind_of(0x8086, 0x29C0, 0x06), None); // host bridge
        assert_eq!(nic_kind_of(0x1234, 0x5678, 0x01), None); // storage
    }

    #[test]
    fn nic_kind_names() {
        assert_eq!(NicKind::E1000 { device_id: 0x100E }.name(), "e1000");
        assert_eq!(NicKind::E1000e { device_id: 0x10D3 }.name(), "e1000e");
        assert_eq!(NicKind::VirtioNet.name(), "virtio-net-pci");
        assert_eq!(NicKind::UnknownNetwork.name(), "unknown-network");
    }

    #[test]
    fn mac_rar_round_trip() {
        // QEMU's default e1000 station address.
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let ral = ral_of_mac(&mac);
        let rah = rah_of_mac(&mac);
        assert_eq!(ral, 0x1200_5452);
        assert_eq!(rah, E1000_RAH_AV | 0x0000_5634);
        assert_eq!(mac_of_rar(ral, rah), mac);
        // The Address-Valid bit is set on program, ignored on decode.
        assert_eq!(mac_of_rar(ral, rah & !E1000_RAH_AV), mac);
    }

    #[test]
    fn tx_descriptor_encoding() {
        let d = tx_desc(0x1_0000, 64);
        assert_eq!(d.addr, 0x1_0000);
        assert_eq!(d.length, 64);
        assert_ne!(d.cmd & TX_CMD_EOP, 0);
        assert_ne!(d.cmd & TX_CMD_IFCS, 0);
        assert_ne!(d.cmd & TX_CMD_RS, 0);
        assert!(!tx_done(&d));
        let mut done = d;
        done.status = TX_STATUS_DD;
        assert!(tx_done(&done));
        // The ring math depends on the 16-byte descriptor size.
        assert_eq!(core::mem::size_of::<TxDesc>(), DESC_SIZE);
        assert_eq!(core::mem::size_of::<RxDesc>(), DESC_SIZE);
        assert_eq!(core::mem::align_of::<TxDesc>(), 8);
    }

    #[test]
    fn rx_descriptor_ready_and_len() {
        let d = rx_desc(0x2_0000);
        assert_eq!(d.addr, 0x2_0000);
        assert!(!rx_ready(&d));
        let mut got = d;
        got.status = RX_STATUS_DD | RX_STATUS_EOP;
        got.length = 1514;
        assert!(rx_ready(&got));
        assert_eq!(rx_len(&got), 1514);
    }

    #[test]
    fn ring_index_wraps() {
        assert_eq!(next_index(0, 32), 1);
        assert_eq!(next_index(31, 32), 0);
        assert_eq!(next_index(7, 8), 0);
        // The suggested ring size meets the 128-byte length granularity.
        assert_eq!((RING_ENTRIES * DESC_SIZE) % 128, 0);
    }

    #[test]
    fn rctl_tctl_init_bits() {
        assert_ne!(RCTL_INIT & E1000_RCTL_EN, 0);
        assert_ne!(RCTL_INIT & E1000_RCTL_BAM, 0);
        assert_ne!(RCTL_INIT & E1000_RCTL_SECRC, 0);
        assert_ne!(TCTL_INIT & E1000_TCTL_EN, 0);
        assert_ne!(TCTL_INIT & E1000_TCTL_PSP, 0);
        // 2048-byte buffers: the BSIZE field (bits 17:16) stays 00b.
        assert_eq!(RCTL_INIT & (0b11 << 16), E1000_RCTL_BSIZE_2048);
    }

    #[test]
    fn format_mac_colon_separated() {
        let mut buf = [0u8; 18];
        assert_eq!(
            format_mac(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56], &mut buf),
            "52:54:00:12:34:56"
        );
        let mut buf = [0u8; 18];
        assert_eq!(format_mac(&[0xFF; 6], &mut buf), "ff:ff:ff:ff:ff:ff");
    }

    #[test]
    fn ethernet_frame_fields() {
        let mut frame = [0u8; 64];
        frame[0..6].copy_from_slice(&[0xFF; 6]); // broadcast destination
        frame[6..12].copy_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]); // source
        frame[12] = 0x08;
        frame[13] = 0x00; // IPv4 EtherType
        assert_eq!(eth_dst(&frame), Some([0xFF; 6]));
        assert_eq!(eth_src(&frame), Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));
        assert_eq!(ethertype(&frame), Some(0x0800));
        assert!(is_broadcast_mac(&eth_dst(&frame).unwrap_or_default()));
        assert!(!is_broadcast_mac(&eth_src(&frame).unwrap_or([0xFF; 6])));
        assert_eq!(ETH_HEADER_LEN, 14);
        // Runt frames carry no fields.
        assert_eq!(eth_dst(&[0u8; 5]), None);
        assert_eq!(eth_src(&[0u8; 11]), None);
        assert_eq!(ethertype(&[0u8; 13]), None);
    }
}

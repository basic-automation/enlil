//! DSDT (Differentiated System Description Table) builder
//!
//! Generates a realistic DSDT with AML bytecode that defines the virtual
//! machine's device topology. Windows parses this to discover:
//! - PCI Express root complex
//! - ISA/LPC bridge
//! - RTC, keyboard controller, COM ports
//! - Processor objects
//! - Power management (_S5 sleep state for shutdown)

use super::aml::{AmlBuilder, ResourceTemplate, opcode};
use super::tables::{AcpiSdtHeader, OemInfo};
use crate::truncate::u32_of;

/// DSDT builder configuration
pub struct DsdtConfig {
    pub vcpu_count: u8,
    pub pci_hole_start: u32,
    pub pci_hole_end: u32,
    pub pci_hole_64_start: u64,
    pub pci_hole_64_size: u64,
    pub com1_port: u16,
    pub com1_irq: u8,
    pub has_hpet: bool,
    pub has_rtc: bool,
    pub has_ps2: bool,
}

impl Default for DsdtConfig {
    fn default() -> Self {
        Self {
            vcpu_count: 4,
            pci_hole_start: 0xC000_0000,
            pci_hole_end: 0xFEBF_FFFF,
            pci_hole_64_start: 0x8_0000_0000,
            pci_hole_64_size: 0x80_0000_0000, // 512 GB
            com1_port: 0x3F8,
            com1_irq: 4,
            has_hpet: true,
            has_rtc: true,
            has_ps2: true,
        }
    }
}

/// DSDT builder
pub struct DsdtBuilder {
    oem: OemInfo,
    config: DsdtConfig,
}

impl DsdtBuilder {
    #[must_use]
    pub fn new(config: DsdtConfig) -> Self {
        Self {
            oem: OemInfo::default(),
            config,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    /// Generate the AML bytecode for the DSDT
    fn generate_aml(&self) -> Vec<u8> {
        let mut aml = AmlBuilder::new();
        Self::build_pic_method(&mut aml);
        self.build_system_bus(&mut aml);
        self.build_processors(&mut aml);
        Self::build_sleep_states(&mut aml);
        aml.into_bytes()
    }

    /// Build the global interrupt-model flag `PICF` and the `_PIC` control method
    /// the OS calls to announce whether it drives the legacy 8259 PICs (`_PIC(0)`)
    /// or the I/O APIC (`_PIC(1)`). The method stores the argument into `PICF`,
    /// exactly as real firmware does — a missing `_PIC` is a VM tell. Our `_PRT`
    /// is currently a static APIC table that ignores `PICF`; wiring a mode-
    /// selecting `_PRT` onto this flag is the documented follow-up.
    fn build_pic_method(aml: &mut AmlBuilder) {
        aml.name_integer(b"PICF", 0);
        let m = aml.method_start(b"_PIC", 1, false);
        // Store(Arg0, PICF): StoreOp Arg0 NameString("PICF").
        aml.raw(&[opcode::STORE_OP, opcode::ARG0]).raw(b"PICF");
        aml.method_end(&m);
    }

    /// Build \_SB scope with PCI root and ISA devices
    fn build_system_bus(&self, aml: &mut AmlBuilder) {
        let sb = aml.scope_start(b"_SB_");

        // PCI0 — PCI Express Root Complex
        self.build_pci_root(aml);

        // HPET — a top-level _SB device so the OS can bind its driver.
        if self.config.has_hpet {
            Self::build_hpet(aml);
        }

        aml.scope_end(&sb);
    }

    /// Build the HPET device (HID `PNP0103`). The OS reads the HPET's MMIO
    /// resources from the dedicated HPET ACPI table; this namespace device lets it
    /// find the timer in the ACPI namespace and bind its HPET driver. Gated on the
    /// `has_hpet` config flag so a guest told the platform has an HPET (in the
    /// FADT/HPET tables) also finds the matching device object here.
    fn build_hpet(aml: &mut AmlBuilder) {
        let hpet = aml.device_start(b"HPET");
        aml.name_string(b"_HID", "PNP0103");
        aml.name_integer(b"_UID", 0);
        // _CRS: the 1 KiB read/write MMIO register block at the fixed HPET base.
        let mut crs = ResourceTemplate::new();
        crs.memory32_fixed(u32_of(super::hpet::HPET_BASE_ADDRESS), 0x400, true);
        aml.name_resource_template(b"_CRS", &crs);
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&hpet);
    }

    /// Build PCI Express Root Complex (PCI0)
    fn build_pci_root(&self, aml: &mut AmlBuilder) {
        let pci0 = aml.device_start(b"PCI0");

        // _HID: PCI Express root
        aml.name_string(b"_HID", "PNP0A08");
        // _CID: PCI compatible
        aml.name_string(b"_CID", "PNP0A03");
        // No _ADR: the host bridge is enumerated through the ACPI namespace by its
        // _HID, and its parent is \_SB (not an enumerable PCI bus), so an _ADR here
        // is meaningless. ACPI §6.1 says a Device must carry either _HID or _ADR but
        // not both; emitting _ADR=0 alongside _HID makes a real ACPI compiler warn
        // (iasl 3073) and is a divergence from how firmware describes a PCI root.
        // The _ADR on the ISA bridge below is correct — it *is* a PCI child function.
        // _UID: 0
        aml.name_integer(b"_UID", 0);
        // _BBN: bus base number 0
        aml.name_integer(b"_BBN", 0);

        // _STA: present and functional
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);

        // _CRS: the resources the host bridge *produces* for bus 0 — the bus-number
        // window, the legacy I/O ports (split around the PCI config aperture), and
        // the 32-/64-bit PCI MMIO holes. Without this Windows cannot enumerate or
        // assign resources to PCI devices below the root.
        let mut crs = ResourceTemplate::new();
        crs.word_bus_number(0x00, 0xFF)
            .word_io(0x0000, 0x0CF7)
            .word_io(0x0D00, 0xFFFF)
            .dword_memory(
                self.config.pci_hole_start,
                self.config.pci_hole_end - self.config.pci_hole_start + 1,
                true,
            )
            .qword_memory(
                self.config.pci_hole_64_start,
                self.config.pci_hole_64_size,
                true,
            );
        aml.name_resource_template(b"_CRS", &crs);

        // PCI interrupt link devices (PNP0C0F) — LNKA..LNKD — that the PIC-mode
        // _PRT routes through. Defined under PCI0 so the _PRT's bare NameSeg
        // references resolve.
        Self::build_pci_link_devices(aml);

        // _PRT — PCI interrupt routing for bus 0.
        Self::build_pci_routing_table(aml);

        // ISA/LPC bridge
        self.build_isa_bridge(aml);

        aml.device_end(&pci0);
    }

    /// The four PCI interrupt link-device names (`PIRQ[A-D]`), in line order.
    const LINK_NAMES: [[u8; 4]; 4] = [*b"LNKA", *b"LNKB", *b"LNKC", *b"LNKD"];

    /// Build the four PCI interrupt **link devices** (`PNP0C0F`), one per `PIRQ`
    /// line, exactly as a real PIIX/ICH DSDT does. Each link carries:
    /// - `_PRS`: the set of ISA IRQs the line *may* be routed to (level, active-low,
    ///   shared — PCI interrupt electrical characteristics);
    /// - `_CRS`: the single IRQ it currently drives — its firmware-default
    ///   [`PIRQ_DEFAULT_IRQS`] entry, so it agrees with the PIC `_PRT` and the
    ///   routing the firmware programs;
    /// - `_STA`/`_DIS`/`_SRS`: present-and-enabled status, plus the disable/set
    ///   methods the OS calls. Routing is fixed in this model, so `_SRS`/`_DIS` are
    ///   accepted no-ops and `_CRS` stays authoritative.
    ///
    /// Their absence (a `_PRT` with only integer GSI/IRQ sources) is itself a
    /// divergence from how every real PIIX/ICH platform describes PCI interrupts.
    fn build_pci_link_devices(aml: &mut AmlBuilder) {
        use crate::interrupt::PIRQ_DEFAULT_IRQS;
        // The IRQs a PIRQ line may be routed to (PCI-routable ISA IRQs).
        const ROUTABLE: &[u8] = &[3, 4, 5, 6, 7, 9, 10, 11, 12, 14, 15];

        for (i, name) in Self::LINK_NAMES.iter().enumerate() {
            let dev = aml.device_start(name);
            aml.name_string(b"_HID", "PNP0C0F");
            aml.name_integer(b"_UID", (i + 1) as u64);

            // _PRS — possible IRQ settings (level, active-low, shared).
            let mut prs = ResourceTemplate::new();
            prs.irq_flags(ROUTABLE, false, true, true);
            aml.name_resource_template(b"_PRS", &prs);

            // _CRS — the IRQ currently driven (the firmware default for this line).
            let mut crs = ResourceTemplate::new();
            crs.irq_flags(&[PIRQ_DEFAULT_IRQS[i]], false, true, true);
            aml.name_resource_template(b"_CRS", &crs);

            // _STA — present, enabled (0x0B = bits 0,1,3).
            let sta = aml.method_start(b"_STA", 0, false);
            aml.return_integer(0x0B);
            aml.method_end(&sta);

            // _DIS / _SRS — accepted no-ops (routing is fixed).
            let dis = aml.method_start(b"_DIS", 0, false);
            aml.method_end(&dis);
            let srs = aml.method_start(b"_SRS", 1, false);
            aml.method_end(&srs);

            aml.device_end(&dev);
        }
    }

    /// Build the PCI interrupt routing table (`_PRT`) for bus 0.
    ///
    /// Emitted as a **mode-selecting method**, matching real firmware:
    ///
    /// ```asl
    /// Method (_PRT, 0) {
    ///     If (PICF) { Return (Package { ...APIC GSIs... }) }
    ///     Return (Package { ...PIC ISA IRQs... })
    /// }
    /// ```
    ///
    /// `PICF` is the interrupt-model flag the OS sets via `_PIC` (0 = 8259 PIC,
    /// 1 = I/O APIC). Until the OS calls `_PIC(1)` the flag is 0 and the method
    /// returns the **PIC-mode** table; after switching to APIC mode it returns the
    /// **APIC-mode** table. A static APIC-only `_PRT` (the previous shape) left a
    /// PIC-mode guest — or the window before `_PIC(1)` — with no usable routing,
    /// itself a divergence from how a real PIIX/ICH DSDT is written.
    ///
    /// - APIC: each `(slot, pin)` swizzles to I/O APIC GSI 16-19 via
    ///   [`PirqRouter::device_gsi`]; `Source = 0`, `SourceIndex` = the GSI.
    /// - PIC: each `(slot, pin)` swizzles onto `PIRQ[A-D]` and routes through the
    ///   matching **link device** `LNK[A-D]` (`Source` = the link, `SourceIndex` =
    ///   0). The OS reads the link's `_CRS` for the actual IRQ — the firmware
    ///   default for that line — so the table, the links, and the routing the
    ///   firmware programs all agree, per ACPI §6.2.13.
    fn build_pci_routing_table(aml: &mut AmlBuilder) {
        use crate::interrupt::PirqRouter;

        let mut apic: Vec<[u64; 4]> = Vec::with_capacity(32 * 4);
        let mut pic: Vec<(u64, u64, [u8; 4])> = Vec::with_capacity(32 * 4);
        for slot in 0u8..32 {
            for prt_pin in 0u8..4 {
                // _PRT pin 0=INTA..3=INTD; the router uses 1=INTA..4=INTD.
                let address = (u64::from(slot) << 16) | 0xFFFF;
                if let Some(gsi) = PirqRouter::device_gsi(slot, prt_pin + 1) {
                    apic.push([address, u64::from(prt_pin), 0, u64::from(gsi)]);
                }
                // PIC mode routes through the link device for this PIRQ line.
                if let Some(line) = PirqRouter::pirq_line(slot, prt_pin + 1) {
                    pic.push((address, u64::from(prt_pin), Self::LINK_NAMES[line]));
                }
            }
        }

        let method = aml.method_start(b"_PRT", 0, false);
        let if_apic = aml.if_name_start(b"PICF");
        aml.return_routing_table(&apic);
        aml.if_end(&if_apic);
        // Fall-through (PICF == 0): PIC mode, routed through the LNK[A-D] devices.
        aml.return_routing_table_via_links(&pic);
        aml.method_end(&method);
    }

    /// Build ISA/LPC bridge under PCI0
    fn build_isa_bridge(&self, aml: &mut AmlBuilder) {
        let isa = aml.device_start(b"ISA_");

        // The ISA/LPC bridge's _ADR must point at the PIIX3 bridge the device bus
        // actually mounts — function 0 of device 1 (00:01.0) on the 440FX/PIIX3
        // chipset — not the ICH-era 1F.0 location. Deriving it from the shared
        // PIIX3_ISA_BRIDGE_BDF keeps the ACPI object and the live bridge from
        // drifting, so the guest's ISA device binds to the real PIRQ-router bridge.
        aml.name_integer(
            b"_ADR",
            u64::from(crate::pcie::PIIX3_ISA_BRIDGE_BDF.acpi_adr()),
        );

        // RTC
        if self.config.has_rtc {
            Self::build_rtc(aml);
        }

        // PS/2 Keyboard Controller
        if self.config.has_ps2 {
            Self::build_ps2(aml);
        }

        // COM1 serial port
        self.build_com1(aml);

        // Motherboard-reserved legacy I/O so the OS doesn't reassign PnP devices
        // onto the fixed controllers.
        Self::build_motherboard_resources(aml);

        aml.device_end(&isa);
    }

    /// Build the PNP motherboard-resources device (HID `PNP0C02`). Its `_CRS`
    /// claims the fixed-function legacy controller I/O that Enlil actually models
    /// (the two 8259 PICs, the 8254 PIT, the two 8237A DMA controllers and DMA
    /// page registers, System Control Ports A/B, and the PIIX ELCR) so the guest's
    /// plug-and-play manager reports them as consumed — matching what a real
    /// chipset's firmware reserves. The RTC/COM/keyboard ports are claimed by
    /// their own device objects above.
    fn build_motherboard_resources(aml: &mut AmlBuilder) {
        let dev = aml.device_start(b"SYSR");
        aml.name_string(b"_HID", "PNP0C02");
        aml.name_integer(b"_UID", 1);
        let mut crs = ResourceTemplate::new();
        crs.io_port(0x0000, 0x10) // 8237A DMA-1 (channels 0-3)
            .io_port(0x0020, 2) // master 8259A
            .io_port(0x0040, 4) // 8254 PIT
            .io_port(0x0061, 1) // System Control Port B (NMI/speaker)
            .io_port(0x0080, 0x10) // DMA page registers
            .io_port(0x0092, 1) // System Control Port A (fast A20/reset)
            .io_port(0x00A0, 2) // slave 8259A
            .io_port(0x00C0, 0x20) // 8237A DMA-2 (channels 4-7)
            .io_port(0x04D0, 2); // PIIX ELCR
        aml.name_resource_template(b"_CRS", &crs);
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&dev);
    }

    /// Build RTC device. `_CRS` reports the CMOS index/data ports (0x70-0x71) and
    /// the periodic/alarm interrupt (IRQ8).
    fn build_rtc(aml: &mut AmlBuilder) {
        let rtc = aml.device_start(b"RTC_");
        aml.name_string(b"_HID", "PNP0B00");
        let mut crs = ResourceTemplate::new();
        crs.io_port(0x70, 2).irq(8);
        aml.name_resource_template(b"_CRS", &crs);
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&rtc);
    }

    /// Build PS/2 keyboard and mouse. The i8042 data/command ports (0x60, 0x64)
    /// live on the keyboard's `_CRS` (with IRQ1); the mouse shares those ports and
    /// reports only its own interrupt (IRQ12), matching real ACPI namespaces.
    fn build_ps2(aml: &mut AmlBuilder) {
        // Keyboard
        let kbd = aml.device_start(b"KBD_");
        aml.name_string(b"_HID", "PNP0303");
        let mut kbd_crs = ResourceTemplate::new();
        kbd_crs.io_port(0x60, 1).io_port(0x64, 1).irq(1);
        aml.name_resource_template(b"_CRS", &kbd_crs);
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&kbd);

        // Mouse
        let mou = aml.device_start(b"MOU_");
        aml.name_string(b"_HID", "PNP0F13");
        let mut mou_crs = ResourceTemplate::new();
        mou_crs.irq(12);
        aml.name_resource_template(b"_CRS", &mou_crs);
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&mou);
    }

    /// Build COM1 serial port. `_CRS` reports the configured I/O window (8 ports)
    /// and IRQ so the OS — Windows in particular — assigns the port resources
    /// from the namespace rather than guessing.
    fn build_com1(&self, aml: &mut AmlBuilder) {
        let com1 = aml.device_start(b"COM1");
        aml.name_string(b"_HID", "PNP0501");
        aml.name_integer(b"_UID", 1);
        let mut crs = ResourceTemplate::new();
        crs.io_port(self.config.com1_port, 8)
            .irq(self.config.com1_irq);
        aml.name_resource_template(b"_CRS", &crs);
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&com1);
    }

    /// Build processor objects (_PR scope)
    fn build_processors(&self, aml: &mut AmlBuilder) {
        let pr = aml.scope_start(b"_PR_");
        for i in 0..self.config.vcpu_count {
            let name = processor_name(i);
            let proc_dev = aml.device_start(&name);
            aml.name_string(b"_HID", "ACPI0007");
            aml.name_integer(b"_UID", u64::from(i));
            let sta = aml.method_start(b"_STA", 0, false);
            aml.return_integer(0x0F);
            aml.method_end(&sta);
            aml.device_end(&proc_dev);
        }
        aml.scope_end(&pr);
    }

    /// Build sleep state objects (\S5 for shutdown)
    fn build_sleep_states(aml: &mut AmlBuilder) {
        // \_S5 (soft off) — required for ACPI shutdown. _Sx must be a *Package*
        // of { PM1a_CNT.SLP_TYP, PM1b_CNT.SLP_TYP, reserved, reserved }; the OS
        // evaluates it and writes element 0 to PM1a_CNT to power off. SLP_TYP = 5
        // matches the value the chipset PM1a model captures as a shutdown request
        // (see enlil_devices::chipset). Emitting it as a bare integer (as before)
        // left the guest with no usable S5 object, breaking ACPI shutdown.
        aml.name_package(b"_S5_", &[5, 5, 0, 0]);
    }

    /// Build the DSDT as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let aml_bytes = self.generate_aml();
        let total_length = 36 + aml_bytes.len();

        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"DSDT", u32_of(total_length), 2, &self.oem);
        buf.extend_from_slice(&header.to_bytes());
        buf.extend_from_slice(&aml_bytes);

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

/// Generate processor name like C000, C001, ..., C00F, C010, etc.
const fn processor_name(index: u8) -> [u8; 4] {
    let hex_chars = b"0123456789ABCDEF";
    [
        b'C',
        hex_chars[((index >> 4) & 0xF) as usize],
        hex_chars[(index & 0xF) as usize],
        b'_',
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsdt_builds() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        assert_eq!(&dsdt[0..4], b"DSDT");
        assert!(dsdt.len() > 36, "DSDT should contain AML data");
    }

    #[test]
    fn dsdt_checksum() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_revision() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        assert_eq!(dsdt[8], 2);
    }

    #[test]
    fn dsdt_contains_pci_hid() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // Should contain PNP0A08 string somewhere in the AML
        let aml = &dsdt[36..];
        let found = aml.windows(7).any(|w| w == b"PNP0A08");
        assert!(found, "DSDT must contain PCI Express root HID");
    }

    #[test]
    fn dsdt_contains_processor_objects() {
        let config = DsdtConfig {
            vcpu_count: 2,
            ..DsdtConfig::default()
        };
        let dsdt = DsdtBuilder::new(config).build();
        let aml = &dsdt[36..];
        // Should contain ACPI0007 (processor device HID)
        let found = aml.windows(8).any(|w| w == b"ACPI0007");
        assert!(found, "DSDT must contain processor device HID");
    }

    #[test]
    fn processor_name_format() {
        assert_eq!(&processor_name(0), b"C00_");
        assert_eq!(&processor_name(1), b"C01_");
        assert_eq!(&processor_name(15), b"C0F_");
        assert_eq!(&processor_name(16), b"C10_");
    }

    #[test]
    fn dsdt_custom_vcpu_count() {
        let config = DsdtConfig {
            vcpu_count: 8,
            ..DsdtConfig::default()
        };
        let dsdt = DsdtBuilder::new(config).build();
        assert!(dsdt.len() > 36);
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_defines_pic_method_and_flag() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // PICF global and the _PIC method both present.
        assert!(
            dsdt.windows(4).any(|w| w == b"PICF"),
            "DSDT must define the PICF interrupt-model flag"
        );
        let pos = dsdt
            .windows(4)
            .position(|w| w == b"_PIC")
            .expect("DSDT must define a _PIC method");
        // _PIC is a Method: the byte before the name is the METHOD_OP's PkgLength,
        // and before that the METHOD_OP. Find the METHOD_OP preceding the name.
        // Simpler: the method's flags byte (argc=1) follows the name.
        let flags = dsdt[pos + 4];
        assert_eq!(flags & 0x07, 1, "_PIC takes one argument");
        // Body contains Store(Arg0, PICF): STORE_OP, ARG0, then "PICF".
        let store = [opcode::STORE_OP, opcode::ARG0, b'P', b'I', b'C', b'F'];
        assert!(
            dsdt.windows(store.len()).any(|w| w == store),
            "_PIC must store its argument into PICF"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_pci_root_has_prt_matching_the_router() {
        use super::super::aml::opcode;
        use crate::interrupt::PirqRouter;
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // _PRT is now a mode-selecting Method: METHOD_OP ... "_PRT" ... and the
        // method body opens with If (PICF).
        let pos = dsdt
            .windows(4)
            .position(|w| w == b"_PRT")
            .expect("PCI0 must carry a _PRT");
        // The method's arg-count flags byte (argc 0) follows the name; the body
        // then opens with IF_OP + PkgLength + the "PICF" predicate.
        let flags = dsdt[pos + 4];
        assert_eq!(flags & 0x07, 0, "_PRT takes no arguments");
        let after = &dsdt[pos + 5..];
        assert_eq!(after[0], opcode::IF_OP, "_PRT body opens with If");
        // After IF_OP comes the PkgLength field (1-3 bytes), then the "PICF"
        // predicate NameString — it appears within the first few bytes of the body.
        assert!(
            after[1..8].windows(4).any(|w| w == b"PICF"),
            "the If predicate is PICF"
        );

        // APIC-mode table (inside the If). Slot 1 INTA -> GSI 17.
        assert_eq!(PirqRouter::device_gsi(1, 1), Some(17));
        let entry = [0x04u8, 0x0C, 0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00, 0x0A, 0x11];
        assert!(
            dsdt.windows(entry.len()).any(|w| w == entry),
            "_PRT must contain the APIC slot-1 INTA -> GSI17 entry"
        );
        // Slot 0 INTA -> GSI16, Word addr 0x0000FFFF.
        let entry0 = [0x04u8, 0x0B, 0xFF, 0xFF, 0x00, 0x00, 0x0A, 0x10];
        assert!(
            dsdt.windows(entry0.len()).any(|w| w == entry0),
            "_PRT must contain the APIC slot-0 INTA -> GSI16 entry"
        );

        // PIC-mode fall-through table routes through the link devices: slot 0 INTA
        // -> PIRQA -> LNKA, encoded { Word 0x0000FFFF, pin ZERO, NameSeg LNKA,
        // SourceIndex ZERO }.
        let pic0 = [0x04u8, 0x0B, 0xFF, 0xFF, 0x00, b'L', b'N', b'K', b'A', 0x00];
        assert!(
            dsdt.windows(pic0.len()).any(|w| w == pic0),
            "_PRT PIC entry must route slot-0 INTA through link LNKA"
        );
        // Slot 1 INTA -> PIRQB -> LNKB, DWord addr 0x0001FFFF.
        let pic1 = [
            0x04u8, 0x0C, 0xFF, 0xFF, 0x01, 0x00, 0x00, b'L', b'N', b'K', b'B', 0x00,
        ];
        assert!(
            dsdt.windows(pic1.len()).any(|w| w == pic1),
            "_PRT PIC entry must route slot-1 INTA through link LNKB"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_defines_pci_interrupt_link_devices() {
        use crate::interrupt::PIRQ_DEFAULT_IRQS;
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // All four PNP0C0F link devices are present.
        assert!(
            dsdt.windows(7).any(|w| w == b"PNP0C0F"),
            "DSDT must define PCI interrupt link devices"
        );
        for name in [b"LNKA", b"LNKB", b"LNKC", b"LNKD"] {
            assert!(
                dsdt.windows(4).any(|w| w == name),
                "link device {name:?} must be defined"
            );
        }
        // LNKA's _CRS reports its firmware-default IRQ (PIRQ_DEFAULT_IRQS[0]=11) as a
        // single-IRQ level/active-low/shared descriptor: 0x23, mask=1<<11, flags 0x18.
        let mask = 1u16 << PIRQ_DEFAULT_IRQS[0];
        let crs = [0x23u8, (mask & 0xFF) as u8, (mask >> 8) as u8, 0x18];
        assert!(
            dsdt.windows(crs.len()).any(|w| w == crs),
            "a link _CRS must report its default IRQ as a level/active-low/shared descriptor"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_pci_root_uses_hid_not_adr() {
        // ACPI §6.1: a Device carries either _HID or _ADR, not both. The PCI host
        // bridge is ACPI-enumerated via _HID, so it must NOT also carry _ADR (a real
        // ACPI compiler warns — iasl 3073 — and it is a firmware-description tell).
        // The only _ADR in the DSDT is the ISA bridge (a genuine PCI child function),
        // so exactly one _ADR name may appear in the whole table.
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        let adr_count = dsdt.windows(4).filter(|w| *w == b"_ADR").count();
        assert_eq!(
            adr_count, 1,
            "only the ISA bridge may carry _ADR; the PCI root must use _HID alone"
        );
        // The ISA bridge's _ADR (00:01.0 -> 0x00010000) is the one that remains.
        assert_eq!(crate::pcie::PIIX3_ISA_BRIDGE_BDF.acpi_adr(), 0x0001_0000);
        let isa_adr = [
            b'_',
            b'A',
            b'D',
            b'R',
            opcode::DWORD_PREFIX,
            0x00,
            0x00,
            0x01,
            0x00,
        ];
        assert!(
            dsdt.windows(isa_adr.len()).any(|w| w == isa_adr),
            "the surviving _ADR must be the ISA bridge at 00:01.0"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_pci_root_has_crs_with_bus_io_and_memory_windows() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // WordBusNumber producing bus 0-0xFF: 0x88, len 13, restype 2.
        let bus = [0x88u8, 0x0D, 0x00, 0x02];
        assert!(
            dsdt.windows(bus.len()).any(|w| w == bus),
            "PCI0 _CRS must produce a bus-number window"
        );
        // DWord memory descriptor (0x87) for the 32-bit hole present.
        assert!(
            dsdt.windows(3).any(|w| w == [0x87u8, 0x17, 0x00]),
            "PCI0 _CRS must contain the 32-bit MMIO window"
        );
        // QWord memory descriptor (0x8A) for the 64-bit hole present.
        assert!(
            dsdt.windows(3).any(|w| w == [0x8Au8, 0x2B, 0x00]),
            "PCI0 _CRS must contain the 64-bit MMIO window"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_s5_is_a_package_yielding_slp_typ_5() {
        use super::super::aml::opcode;
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // Find Name(_S5_, ...) and confirm it is a PackageOp (0x12), not an integer.
        let pos = dsdt
            .windows(4)
            .position(|w| w == b"_S5_")
            .expect("DSDT must define _S5");
        // After NAME_OP + name come the object bytes.
        assert_eq!(dsdt[pos + 4], opcode::PACKAGE_OP, "_S5 must be a Package");
        // The package must contain SLP_TYP = 5 (BYTE_PREFIX 0x05) for PM1a/PM1b.
        let slp = [0x0Au8, 0x05, 0x0A, 0x05];
        assert!(
            dsdt[pos..].windows(slp.len()).any(|w| w == slp),
            "_S5 package must yield SLP_TYP 5 for PM1a and PM1b"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_com1_has_crs_with_configured_port_and_irq() {
        let config = DsdtConfig {
            com1_port: 0x3F8,
            com1_irq: 4,
            ..DsdtConfig::default()
        };
        let dsdt = DsdtBuilder::new(config).build();
        // The _CRS name and a fixed I/O Port Descriptor (0x47) for 0x3F8 must
        // appear in the AML, followed by an IRQ descriptor (0x23) with IRQ4.
        let found_crs = dsdt.windows(4).any(|w| w == b"_CRS");
        assert!(found_crs, "COM1 must carry a _CRS");
        // I/O Port Descriptor: 0x47, info, min(LE)=F8 03, max=F8 03, align, len=8.
        let io = [0x47u8, 0x01, 0xF8, 0x03, 0xF8, 0x03, 0x01, 0x08];
        assert!(
            dsdt.windows(io.len()).any(|w| w == io),
            "COM1 _CRS must contain the 0x3F8/len-8 I/O descriptor"
        );
        // IRQ descriptor for IRQ4: 0x23, mask=0x0010, flags=0x01.
        let irq = [0x23u8, 0x10, 0x00, 0x01];
        assert!(
            dsdt.windows(irq.len()).any(|w| w == irq),
            "COM1 _CRS must contain the IRQ4 descriptor"
        );
        // Checksum still valid.
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_motherboard_resources_claim_fixed_legacy_io() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // PNP0C02 motherboard-resources HID present.
        assert!(
            dsdt.windows(7).any(|w| w == b"PNP0C02"),
            "DSDT must contain the motherboard-resources device"
        );
        // PIT window 0x40 len 4.
        let pit = [0x47u8, 0x01, 0x40, 0x00, 0x40, 0x00, 0x01, 0x04];
        assert!(
            dsdt.windows(pit.len()).any(|w| w == pit),
            "SYSR _CRS must claim the PIT I/O window"
        );
        // ELCR window 0x4D0 len 2.
        let elcr = [0x47u8, 0x01, 0xD0, 0x04, 0xD0, 0x04, 0x01, 0x02];
        assert!(
            dsdt.windows(elcr.len()).any(|w| w == elcr),
            "SYSR _CRS must claim the ELCR I/O window"
        );
        // 8237A DMA-1 window 0x0000 len 0x10.
        let dma1 = [0x47u8, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x10];
        assert!(
            dsdt.windows(dma1.len()).any(|w| w == dma1),
            "SYSR _CRS must claim the DMA-1 I/O window"
        );
        // 8237A DMA-2 window 0x00C0 len 0x20.
        let dma2 = [0x47u8, 0x01, 0xC0, 0x00, 0xC0, 0x00, 0x01, 0x20];
        assert!(
            dsdt.windows(dma2.len()).any(|w| w == dma2),
            "SYSR _CRS must claim the DMA-2 I/O window"
        );
        // DMA page-register window 0x0080 len 0x10.
        let dmapg = [0x47u8, 0x01, 0x80, 0x00, 0x80, 0x00, 0x01, 0x10];
        assert!(
            dsdt.windows(dmapg.len()).any(|w| w == dmapg),
            "SYSR _CRS must claim the DMA page-register I/O window"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_rtc_and_ps2_carry_crs() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // RTC: I/O 0x70 len 2 + IRQ8.
        let rtc_io = [0x47u8, 0x01, 0x70, 0x00, 0x70, 0x00, 0x01, 0x02];
        assert!(
            dsdt.windows(rtc_io.len()).any(|w| w == rtc_io),
            "RTC _CRS must contain the 0x70/len-2 I/O descriptor"
        );
        let rtc_irq = [0x23u8, 0x00, 0x01, 0x01]; // IRQ8 → mask 0x0100
        assert!(
            dsdt.windows(rtc_irq.len()).any(|w| w == rtc_irq),
            "RTC _CRS must contain the IRQ8 descriptor"
        );
        // Keyboard: I/O 0x60 len 1 and 0x64 len 1.
        let kbd_io_60 = [0x47u8, 0x01, 0x60, 0x00, 0x60, 0x00, 0x01, 0x01];
        let kbd_io_64 = [0x47u8, 0x01, 0x64, 0x00, 0x64, 0x00, 0x01, 0x01];
        assert!(dsdt.windows(kbd_io_60.len()).any(|w| w == kbd_io_60));
        assert!(dsdt.windows(kbd_io_64.len()).any(|w| w == kbd_io_64));
        // Mouse: IRQ12 → mask 0x1000.
        let mou_irq = [0x23u8, 0x00, 0x10, 0x01];
        assert!(
            dsdt.windows(mou_irq.len()).any(|w| w == mou_irq),
            "mouse _CRS must contain the IRQ12 descriptor"
        );
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_describes_the_hpet_when_present() {
        // With has_hpet (the default), the namespace must contain the HPET device
        // (HID PNP0103) so the OS finds the timer it was told about in the tables.
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        let found = dsdt.windows(7).any(|w| w == b"PNP0103");
        assert!(found, "DSDT must contain the HPET device HID");

        // The HPET device carries a Memory32Fixed _CRS for its 1 KiB block.
        // 0x86, body-len 0x0009, info=1(rw), base 0xFED00000, len 0x400.
        let mem = [
            0x86u8, 0x09, 0x00, 0x01, 0x00, 0x00, 0xD0, 0xFE, 0x00, 0x04, 0x00, 0x00,
        ];
        assert!(
            dsdt.windows(mem.len()).any(|w| w == mem),
            "HPET _CRS must contain the Memory32Fixed descriptor"
        );

        // And with has_hpet cleared, the device is absent.
        let config = DsdtConfig {
            has_hpet: false,
            ..DsdtConfig::default()
        };
        let no_hpet = DsdtBuilder::new(config).build();
        assert!(
            !no_hpet.windows(7).any(|w| w == b"PNP0103"),
            "no HPET device when has_hpet is cleared"
        );
        // The checksum must still be valid in both cases.
        let sum: u8 = no_hpet.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }
}

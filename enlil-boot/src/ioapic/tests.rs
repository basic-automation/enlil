#![allow(clippy::cast_possible_truncation)] // fixture arithmetic on small bounded values

use super::*;

/// A local APIC that is software-enabled, so raised vectors can be seen.
fn lapic() -> Lapic {
    let mut l = Lapic::new(1_000_000_000);
    l.write(0xF0, 4, 0x1FF, 0);
    l
}

/// Program redirection entry `pin` (low dword; destination left at 0).
fn program(io: &mut IoApic, l: &mut Lapic, pin: u8, low: u32) {
    io.write(OFF_IOREGSEL, 4, u64::from(REG_REDIR + 2 * pin), l);
    io.write(OFF_IOWIN, 4, u64::from(low), l);
}

fn entry(io: &IoApic, pin: u8) -> u64 {
    let lo = {
        let mut x = io.clone();
        x.index = REG_REDIR + 2 * pin;
        u64::from(x.read_register())
    };
    let hi = {
        let mut x = io.clone();
        x.index = REG_REDIR + 2 * pin + 1;
        u64::from(x.read_register())
    };
    (hi << 32) | lo
}

/// Take whatever the local APIC would deliver, as the CPU does.
fn take(l: &mut Lapic) -> Option<u8> {
    l.ack()
}

#[test]
fn reset_state_is_ids_version_and_all_pins_masked() {
    let io = IoApic::new();
    let mut x = io.clone();
    x.index = REG_VER;
    assert_eq!(x.read_register(), 0x0017_0011, "version 0x11, 24 pins");
    for pin in 0..PINS as u8 {
        assert_ne!(entry(&io, pin) & R_MASKED, 0, "pin {pin} masked at reset");
    }
    assert!(IoApic::contains(0xFEC0_0010) && !IoApic::contains(0xFEC0_1000));
}

#[test]
fn the_index_and_window_registers_read_and_write_entries() {
    let mut io = IoApic::new();
    let mut l = lapic();
    // IOREGSEL reads back; IOWIN follows it.
    io.write(OFF_IOREGSEL, 4, 0x01, &mut l);
    assert_eq!(io.read(OFF_IOREGSEL, 4), 1);
    assert_eq!(io.read(OFF_IOWIN, 4), 0x0017_0011);
    // Program pin 5: vector 0x41, level-triggered, active low, unmasked; dest 3.
    program(
        &mut io,
        &mut l,
        5,
        0x41 | R_LEVEL as u32 | R_POLARITY_LOW as u32,
    );
    io.write(OFF_IOREGSEL, 4, u64::from(REG_REDIR + 11), &mut l);
    io.write(OFF_IOWIN, 4, 0x0300_0000, &mut l);
    let e = entry(&io, 5);
    assert_eq!(e & 0xFF, 0x41);
    assert_ne!(e & R_LEVEL, 0);
    assert_ne!(e & R_POLARITY_LOW, 0);
    assert_eq!(e & R_MASKED, 0);
    assert_eq!(e >> 56, 3, "destination");
    // The ID register keeps four bits in 27:24.
    io.write(OFF_IOREGSEL, 4, 0, &mut l);
    io.write(OFF_IOWIN, 4, 0x0A00_0000, &mut l);
    assert_eq!(io.read(OFF_IOWIN, 4), 0x0A00_0000);
    // Sub-dword and unknown-offset writes are ignored.
    io.write(OFF_IOWIN, 1, 0xFF, &mut l);
    io.write(0x80, 4, 0xFF, &mut l);
    assert_eq!(io.read(0x20, 4), 0);
}

#[test]
fn remote_irr_and_delivery_status_are_not_writable() {
    let mut io = IoApic::new();
    let mut l = lapic();
    program(
        &mut io,
        &mut l,
        0,
        (R_REMOTE_IRR | (1 << 12) | R_LEVEL) as u32 | 0x30,
    );
    let e = entry(&io, 0);
    assert_eq!(e & R_REMOTE_IRR, 0);
    assert_eq!(e & (1 << 12), 0);
    assert_eq!(e & 0xFF, 0x30);
}

#[test]
fn an_edge_interrupt_is_delivered_once_per_rising_edge() {
    let mut io = IoApic::new();
    let mut l = lapic();
    program(&mut io, &mut l, 2, 0x30); // unmasked edge, vector 0x30
    io.set_irq(2, true, &mut l);
    assert_eq!(take(&mut l), Some(0x30));
    // Holding the line high delivers nothing more.
    io.set_irq(2, true, &mut l);
    assert_eq!(l.pending_vector(), None);
    // A new edge does.
    l.write(0xB0, 4, 0, 0); // EOI
    io.set_irq(2, false, &mut l);
    io.set_irq(2, true, &mut l);
    assert_eq!(take(&mut l), Some(0x30));
}

#[test]
fn an_edge_that_arrives_while_masked_is_delivered_on_unmask() {
    let mut io = IoApic::new();
    let mut l = lapic();
    program(&mut io, &mut l, 3, 0x31 | R_MASKED as u32);
    io.set_irq(3, true, &mut l);
    assert_eq!(l.pending_vector(), None, "masked");
    program(&mut io, &mut l, 3, 0x31); // unmask
    assert_eq!(take(&mut l), Some(0x31), "the latched edge goes out");
    assert_eq!(l.pending_vector(), None, "and only once");
}

#[test]
fn a_level_interrupt_redelivers_after_eoi_while_the_line_stays_asserted() {
    let mut io = IoApic::new();
    let mut l = lapic();
    program(&mut io, &mut l, 16, 0x51 | R_LEVEL as u32);
    io.set_irq(16, true, &mut l);
    assert_eq!(take(&mut l), Some(0x51));
    assert_ne!(
        entry(&io, 16) & R_REMOTE_IRR,
        0,
        "Remote IRR while in service"
    );
    // The device is still asserting: nothing new until the OS says EOI.
    io.set_irq(16, true, &mut l);
    assert_eq!(l.pending_vector(), None);
    // The local APIC's EOI broadcast reaches the I/O APIC.
    let v = l.write(0xB0, 4, 0, 0).expect("the EOI retired a vector");
    io.eoi(v, &mut l);
    assert_eq!(take(&mut l), Some(0x51), "still asserted: delivered again");
    assert_ne!(entry(&io, 16) & R_REMOTE_IRR, 0, "and in service again");
    // Now the device deasserts; the next EOI leaves nothing pending.
    io.set_irq(16, false, &mut l);
    let v = l.write(0xB0, 4, 0, 0).unwrap();
    io.eoi(v, &mut l);
    assert_eq!(l.pending_vector(), None);
    assert_eq!(
        entry(&io, 16) & R_REMOTE_IRR,
        0,
        "Remote IRR clears once the line is low"
    );
}

#[test]
fn the_polarity_bit_is_stored_but_devices_drive_pins_logically() {
    let mut io = IoApic::new();
    let mut l = lapic();
    // An active-low level interrupt, as PCI INTx pins are programmed.
    program(
        &mut io,
        &mut l,
        18,
        0x52 | R_LEVEL as u32 | R_POLARITY_LOW as u32,
    );
    assert_ne!(entry(&io, 18) & R_POLARITY_LOW, 0, "read back as written");
    // An idle device (not requesting) must not look asserted just because the
    // pin is active-low: this raised "irq nobody cared" in a real guest.
    io.set_irq(18, false, &mut l);
    assert_eq!(l.pending_vector(), None);
    // Requesting delivers; withdrawing the request lets the EOI settle.
    io.set_irq(18, true, &mut l);
    assert_eq!(take(&mut l), Some(0x52));
    io.set_irq(18, false, &mut l);
    let v = l.write(0xB0, 4, 0, 0).unwrap();
    io.eoi(v, &mut l);
    assert_eq!(l.pending_vector(), None);
}

#[test]
fn eoi_for_an_unrelated_vector_changes_nothing() {
    let mut io = IoApic::new();
    let mut l = lapic();
    program(&mut io, &mut l, 17, 0x53 | R_LEVEL as u32);
    io.set_irq(17, true, &mut l);
    take(&mut l);
    io.eoi(0x99, &mut l);
    assert_ne!(entry(&io, 17) & R_REMOTE_IRR, 0);
}

#[test]
fn only_fixed_and_lowest_priority_delivery_reach_the_cpu() {
    let mut io = IoApic::new();
    let mut l = lapic();
    program(&mut io, &mut l, 1, 0x40 | (0b100 << 8)); // NMI
    io.set_irq(1, true, &mut l);
    assert_eq!(l.pending_vector(), None);
    program(&mut io, &mut l, 4, 0x44 | (0b001 << 8)); // lowest priority
    io.set_irq(4, true, &mut l);
    assert_eq!(take(&mut l), Some(0x44));
}

#[test]
fn out_of_range_pins_are_ignored() {
    let mut io = IoApic::new();
    let mut l = lapic();
    io.set_irq(200, true, &mut l);
    assert_eq!(l.pending_vector(), None);
}

#[test]
fn switching_a_pin_to_edge_clears_remote_irr() {
    let mut io = IoApic::new();
    let mut l = lapic();
    program(&mut io, &mut l, 6, 0x60 | R_LEVEL as u32);
    io.set_irq(6, true, &mut l);
    take(&mut l);
    assert_ne!(entry(&io, 6) & R_REMOTE_IRR, 0);
    program(&mut io, &mut l, 6, 0x60); // edge
    assert_eq!(entry(&io, 6) & R_REMOTE_IRR, 0);
}

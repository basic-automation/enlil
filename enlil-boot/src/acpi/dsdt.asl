/*
 * The machine description (DSDT) the guest OS reads.
 *
 * An i440FX-style PC: one PCI root bridge, the ISA devices the platform model
 * has, the processor, and the soft-off sleep state. PCI interrupts are routed
 * for APIC mode only: INTx of slot s lands on GSI 16 + ((s + pin) & 3), the
 * same swizzle the PIIX3 routing model uses (see platform.rs).
 *
 * The window sizes in PCI0._CRS are the ones OVMF's PCI host bridge uses
 * (I/O 0xC000-0xFFFF, memory 0x80000000-0xFBFFFFFF, and 0x380000000000 up for
 * 64-bit BARs); an OS rejects devices whose BARs fall outside them.
 *
 * Compile with scripts/build-dsdt.sh (iasl); the result is committed as dsdt.aml.
 */
DefinitionBlock ("", "DSDT", 2, "ENLIL ", "ENLILVM ", 0x00000001)
{
    Name (PICF, Zero)
    Method (_PIC, 1, NotSerialized)
    {
        PICF = Arg0
    }

    /* Soft-off is sleep type 0 (what the PM1_CNT model treats as S5). */
    Name (_S5, Package (0x04) { Zero, Zero, Zero, Zero })

    Scope (\_SB)
    {
        Device (CPU0)
        {
            Name (_HID, "ACPI0007")
            Name (_UID, Zero)
            Method (_STA, 0, NotSerialized)
            {
                Return (0x0F)
            }
        }

        /* Fixed resources that are not PCI: interrupt controllers, PM block, flash. */
        Device (MBRD)
        {
            Name (_HID, EisaId ("PNP0C02"))
            Name (_UID, One)
            Name (_CRS, ResourceTemplate ()
            {
                IO (Decode16, 0xB000, 0xB000, 0x01, 0x40)
                Memory32Fixed (ReadWrite, 0xFEC00000, 0x00001000)
                Memory32Fixed (ReadWrite, 0xFEE00000, 0x00001000)
                Memory32Fixed (ReadWrite, 0xFFC00000, 0x00400000)
            })
        }

        Device (PCI0)
        {
            Name (_HID, EisaId ("PNP0A03"))
            Name (_UID, Zero)
            Name (_BBN, Zero)

            Name (_CRS, ResourceTemplate ()
            {
                WordBusNumber (ResourceProducer, MinFixed, MaxFixed, PosDecode,
                    0x0000, 0x0000, 0x00FF, 0x0000, 0x0100)
                IO (Decode16, 0x0CF8, 0x0CF8, 0x01, 0x08)
                WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                    0x0000, 0x0000, 0x0CF7, 0x0000, 0x0CF8)
                WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
                    0x0000, 0x0D00, 0xFFFF, 0x0000, 0xF300)
                DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, Cacheable, ReadWrite,
                    0x00000000, 0x000A0000, 0x000BFFFF, 0x00000000, 0x00020000)
                DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                    0x00000000, 0x80000000, 0xFBFFFFFF, 0x00000000, 0x7C000000)
                QWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
                    0x0000000000000000, 0x0000380000000000, 0x00003FFFFFFFFFFF,
                    0x0000000000000000, 0x0000080000000000)
            })

            Name (_PRT, Package ()
            {
                    Package () { 0x0001FFFF, 0x00, Zero, 0x11 },
                    Package () { 0x0001FFFF, 0x01, Zero, 0x12 },
                    Package () { 0x0001FFFF, 0x02, Zero, 0x13 },
                    Package () { 0x0001FFFF, 0x03, Zero, 0x10 },
                    Package () { 0x0002FFFF, 0x00, Zero, 0x12 },
                    Package () { 0x0002FFFF, 0x01, Zero, 0x13 },
                    Package () { 0x0002FFFF, 0x02, Zero, 0x10 },
                    Package () { 0x0002FFFF, 0x03, Zero, 0x11 },
                    Package () { 0x0003FFFF, 0x00, Zero, 0x13 },
                    Package () { 0x0003FFFF, 0x01, Zero, 0x10 },
                    Package () { 0x0003FFFF, 0x02, Zero, 0x11 },
                    Package () { 0x0003FFFF, 0x03, Zero, 0x12 },
                    Package () { 0x0004FFFF, 0x00, Zero, 0x10 },
                    Package () { 0x0004FFFF, 0x01, Zero, 0x11 },
                    Package () { 0x0004FFFF, 0x02, Zero, 0x12 },
                    Package () { 0x0004FFFF, 0x03, Zero, 0x13 },
                    Package () { 0x0005FFFF, 0x00, Zero, 0x11 },
                    Package () { 0x0005FFFF, 0x01, Zero, 0x12 },
                    Package () { 0x0005FFFF, 0x02, Zero, 0x13 },
                    Package () { 0x0005FFFF, 0x03, Zero, 0x10 },
                    Package () { 0x0006FFFF, 0x00, Zero, 0x12 },
                    Package () { 0x0006FFFF, 0x01, Zero, 0x13 },
                    Package () { 0x0006FFFF, 0x02, Zero, 0x10 },
                    Package () { 0x0006FFFF, 0x03, Zero, 0x11 },
                    Package () { 0x0007FFFF, 0x00, Zero, 0x13 },
                    Package () { 0x0007FFFF, 0x01, Zero, 0x10 },
                    Package () { 0x0007FFFF, 0x02, Zero, 0x11 },
                    Package () { 0x0007FFFF, 0x03, Zero, 0x12 },
                    Package () { 0x0008FFFF, 0x00, Zero, 0x10 },
                    Package () { 0x0008FFFF, 0x01, Zero, 0x11 },
                    Package () { 0x0008FFFF, 0x02, Zero, 0x12 },
                    Package () { 0x0008FFFF, 0x03, Zero, 0x13 },
                    Package () { 0x0009FFFF, 0x00, Zero, 0x11 },
                    Package () { 0x0009FFFF, 0x01, Zero, 0x12 },
                    Package () { 0x0009FFFF, 0x02, Zero, 0x13 },
                    Package () { 0x0009FFFF, 0x03, Zero, 0x10 },
                    Package () { 0x000AFFFF, 0x00, Zero, 0x12 },
                    Package () { 0x000AFFFF, 0x01, Zero, 0x13 },
                    Package () { 0x000AFFFF, 0x02, Zero, 0x10 },
                    Package () { 0x000AFFFF, 0x03, Zero, 0x11 },
                    Package () { 0x000BFFFF, 0x00, Zero, 0x13 },
                    Package () { 0x000BFFFF, 0x01, Zero, 0x10 },
                    Package () { 0x000BFFFF, 0x02, Zero, 0x11 },
                    Package () { 0x000BFFFF, 0x03, Zero, 0x12 },
                    Package () { 0x000CFFFF, 0x00, Zero, 0x10 },
                    Package () { 0x000CFFFF, 0x01, Zero, 0x11 },
                    Package () { 0x000CFFFF, 0x02, Zero, 0x12 },
                    Package () { 0x000CFFFF, 0x03, Zero, 0x13 },
                    Package () { 0x000DFFFF, 0x00, Zero, 0x11 },
                    Package () { 0x000DFFFF, 0x01, Zero, 0x12 },
                    Package () { 0x000DFFFF, 0x02, Zero, 0x13 },
                    Package () { 0x000DFFFF, 0x03, Zero, 0x10 },
                    Package () { 0x000EFFFF, 0x00, Zero, 0x12 },
                    Package () { 0x000EFFFF, 0x01, Zero, 0x13 },
                    Package () { 0x000EFFFF, 0x02, Zero, 0x10 },
                    Package () { 0x000EFFFF, 0x03, Zero, 0x11 },
                    Package () { 0x000FFFFF, 0x00, Zero, 0x13 },
                    Package () { 0x000FFFFF, 0x01, Zero, 0x10 },
                    Package () { 0x000FFFFF, 0x02, Zero, 0x11 },
                    Package () { 0x000FFFFF, 0x03, Zero, 0x12 },
                    Package () { 0x0010FFFF, 0x00, Zero, 0x10 },
                    Package () { 0x0010FFFF, 0x01, Zero, 0x11 },
                    Package () { 0x0010FFFF, 0x02, Zero, 0x12 },
                    Package () { 0x0010FFFF, 0x03, Zero, 0x13 },
                    Package () { 0x0011FFFF, 0x00, Zero, 0x11 },
                    Package () { 0x0011FFFF, 0x01, Zero, 0x12 },
                    Package () { 0x0011FFFF, 0x02, Zero, 0x13 },
                    Package () { 0x0011FFFF, 0x03, Zero, 0x10 },
                    Package () { 0x0012FFFF, 0x00, Zero, 0x12 },
                    Package () { 0x0012FFFF, 0x01, Zero, 0x13 },
                    Package () { 0x0012FFFF, 0x02, Zero, 0x10 },
                    Package () { 0x0012FFFF, 0x03, Zero, 0x11 },
                    Package () { 0x0013FFFF, 0x00, Zero, 0x13 },
                    Package () { 0x0013FFFF, 0x01, Zero, 0x10 },
                    Package () { 0x0013FFFF, 0x02, Zero, 0x11 },
                    Package () { 0x0013FFFF, 0x03, Zero, 0x12 },
                    Package () { 0x0014FFFF, 0x00, Zero, 0x10 },
                    Package () { 0x0014FFFF, 0x01, Zero, 0x11 },
                    Package () { 0x0014FFFF, 0x02, Zero, 0x12 },
                    Package () { 0x0014FFFF, 0x03, Zero, 0x13 },
                    Package () { 0x0015FFFF, 0x00, Zero, 0x11 },
                    Package () { 0x0015FFFF, 0x01, Zero, 0x12 },
                    Package () { 0x0015FFFF, 0x02, Zero, 0x13 },
                    Package () { 0x0015FFFF, 0x03, Zero, 0x10 },
                    Package () { 0x0016FFFF, 0x00, Zero, 0x12 },
                    Package () { 0x0016FFFF, 0x01, Zero, 0x13 },
                    Package () { 0x0016FFFF, 0x02, Zero, 0x10 },
                    Package () { 0x0016FFFF, 0x03, Zero, 0x11 },
                    Package () { 0x0017FFFF, 0x00, Zero, 0x13 },
                    Package () { 0x0017FFFF, 0x01, Zero, 0x10 },
                    Package () { 0x0017FFFF, 0x02, Zero, 0x11 },
                    Package () { 0x0017FFFF, 0x03, Zero, 0x12 },
                    Package () { 0x0018FFFF, 0x00, Zero, 0x10 },
                    Package () { 0x0018FFFF, 0x01, Zero, 0x11 },
                    Package () { 0x0018FFFF, 0x02, Zero, 0x12 },
                    Package () { 0x0018FFFF, 0x03, Zero, 0x13 },
                    Package () { 0x0019FFFF, 0x00, Zero, 0x11 },
                    Package () { 0x0019FFFF, 0x01, Zero, 0x12 },
                    Package () { 0x0019FFFF, 0x02, Zero, 0x13 },
                    Package () { 0x0019FFFF, 0x03, Zero, 0x10 },
                    Package () { 0x001AFFFF, 0x00, Zero, 0x12 },
                    Package () { 0x001AFFFF, 0x01, Zero, 0x13 },
                    Package () { 0x001AFFFF, 0x02, Zero, 0x10 },
                    Package () { 0x001AFFFF, 0x03, Zero, 0x11 },
                    Package () { 0x001BFFFF, 0x00, Zero, 0x13 },
                    Package () { 0x001BFFFF, 0x01, Zero, 0x10 },
                    Package () { 0x001BFFFF, 0x02, Zero, 0x11 },
                    Package () { 0x001BFFFF, 0x03, Zero, 0x12 },
                    Package () { 0x001CFFFF, 0x00, Zero, 0x10 },
                    Package () { 0x001CFFFF, 0x01, Zero, 0x11 },
                    Package () { 0x001CFFFF, 0x02, Zero, 0x12 },
                    Package () { 0x001CFFFF, 0x03, Zero, 0x13 },
                    Package () { 0x001DFFFF, 0x00, Zero, 0x11 },
                    Package () { 0x001DFFFF, 0x01, Zero, 0x12 },
                    Package () { 0x001DFFFF, 0x02, Zero, 0x13 },
                    Package () { 0x001DFFFF, 0x03, Zero, 0x10 },
                    Package () { 0x001EFFFF, 0x00, Zero, 0x12 },
                    Package () { 0x001EFFFF, 0x01, Zero, 0x13 },
                    Package () { 0x001EFFFF, 0x02, Zero, 0x10 },
                    Package () { 0x001EFFFF, 0x03, Zero, 0x11 },
                    Package () { 0x001FFFFF, 0x00, Zero, 0x13 },
                    Package () { 0x001FFFFF, 0x01, Zero, 0x10 },
                    Package () { 0x001FFFFF, 0x02, Zero, 0x11 },
                    Package () { 0x001FFFFF, 0x03, Zero, 0x12 }
            })

            Device (ISA)
            {
                Name (_ADR, 0x00010000)

                Device (RTC)
                {
                    Name (_HID, EisaId ("PNP0B00"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x0070, 0x0070, 0x01, 0x02)
                        IRQNoFlags () {8}
                    })
                }

                Device (PIC)
                {
                    Name (_HID, EisaId ("PNP0000"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x0020, 0x0020, 0x01, 0x02)
                        IO (Decode16, 0x00A0, 0x00A0, 0x01, 0x02)
                        IRQNoFlags () {2}
                    })
                }

                Device (TIMR)
                {
                    Name (_HID, EisaId ("PNP0100"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x0040, 0x0040, 0x01, 0x04)
                        IRQNoFlags () {0}
                    })
                }

                Device (SPKR)
                {
                    Name (_HID, EisaId ("PNP0800"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x0061, 0x0061, 0x01, 0x01)
                    })
                }

                Device (MATH)
                {
                    Name (_HID, EisaId ("PNP0C04"))
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x00F0, 0x00F0, 0x01, 0x10)
                        IRQNoFlags () {13}
                    })
                }

                Device (COM1)
                {
                    Name (_HID, EisaId ("PNP0501"))
                    Name (_UID, One)
                    Method (_STA, 0, NotSerialized)
                    {
                        Return (0x0F)
                    }
                    Name (_CRS, ResourceTemplate ()
                    {
                        IO (Decode16, 0x03F8, 0x03F8, 0x01, 0x08)
                        IRQNoFlags () {4}
                    })
                }
            }
        }
    }
}

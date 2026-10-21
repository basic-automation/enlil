//! Decoding the faulting instruction of a guest MMIO access.
//!
//! A guest touching a device register takes a nested page fault that says
//! *where* (the guest-physical address) but not *what*: to emulate the access
//! the hypervisor must know the access size, whether it is a load or a store,
//! which register is involved, and how long the instruction is so execution can
//! resume after it. This module recovers that from the instruction bytes.
//!
//! It deliberately decodes only the plain data-movement forms compilers emit
//! for `readb/readw/readl/readq` and `writeb/writew/writel/writeq`:
//!
//! | opcode            | meaning                                   |
//! |-------------------|-------------------------------------------|
//! | `88` / `89`       | `mov r/m, r` (store)                      |
//! | `8A` / `8B`       | `mov r, r/m` (load)                       |
//! | `C6 /0` / `C7 /0` | `mov r/m, imm` (store immediate)          |
//! | `0F B6/B7/BE/BF`  | `movzx` / `movsx` loads                   |
//!
//! Anything else returns `None` and the caller stops with the raw bytes, so an
//! instruction the decoder does not know is reported rather than guessed at.
//!
//! [`translate_gva`] walks the guest's own 4-level page tables to find the
//! guest-physical address of the instruction pointer, so the bytes can be read
//! from guest RAM. Both pieces are pure.

/// What kind of access an instruction makes to memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    /// `reg <- memory`. `dest_size` is the destination operand size; a narrower
    /// memory read is zero- or sign-extended to it.
    Load {
        /// Destination register number (0-15, x86 encoding order).
        reg: u8,
        /// The destination is `AH/CH/DH/BH` (8-bit access, no REX, reg 4-7).
        high8: bool,
        /// Sign-extend (`movsx`) instead of zero-extend.
        sign_extend: bool,
        /// Destination operand size in bytes (1, 2, 4 or 8).
        dest_size: u8,
    },
    /// `memory <- reg`.
    Store {
        /// Source register number (0-15).
        reg: u8,
        /// The source is `AH/CH/DH/BH`.
        high8: bool,
    },
    /// `memory <- immediate`.
    StoreImm {
        /// The (sign-extended) immediate.
        value: u64,
    },
}

/// A decoded memory-access instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemAccess {
    /// Total instruction length in bytes (how far to advance `RIP`).
    pub len: usize,
    /// Bytes moved to/from memory (1, 2, 4 or 8).
    pub size: u8,
    /// Direction and operands.
    pub kind: AccessKind,
}

/// Legacy prefixes and `REX` seen before the opcode.
#[derive(Clone, Copy)]
struct Prefixes {
    /// A `66` operand-size prefix was present.
    opsize16: bool,
    /// The `REX` byte (`0` if none).
    rex: u8,
}

impl Prefixes {
    const fn rex_w(self) -> bool {
        self.rex & 8 != 0
    }

    /// `REX.R`, shifted into the register-number high bit.
    const fn rex_r(self) -> u8 {
        (self.rex & 4) << 1
    }

    /// The default operand size in bytes.
    const fn operand_size(self) -> u8 {
        if self.rex_w() {
            8
        } else if self.opsize16 {
            2
        } else {
            4
        }
    }
}

/// Consume the prefixes at the start of `bytes`; returns them and the index of
/// the opcode byte.
fn parse_prefixes(bytes: &[u8]) -> (Prefixes, usize) {
    let mut i = 0;
    let mut opsize16 = false;
    // Operand-size, address-size, segment overrides, lock/rep (the last two are
    // not valid on the forms decoded here but are harmless).
    while let Some(&b) = bytes.get(i) {
        match b {
            0x66 => opsize16 = true,
            0x67 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0xF0 | 0xF2 | 0xF3 => {}
            _ => break,
        }
        i += 1;
    }
    let mut rex = 0u8;
    if let Some(&b) = bytes.get(i)
        && b & 0xF0 == 0x40
    {
        rex = b;
        i += 1;
    }
    (Prefixes { opsize16, rex }, i)
}

/// What the opcode does, before the `ModRM` operands are known.
#[derive(Clone, Copy)]
enum Shape {
    Store,
    Load { dest: u8, sign: bool },
    Imm { imm_len: usize },
}

/// Decode the opcode at `bytes[i]`: the memory access size, its shape, and the
/// index just past the opcode.
fn parse_opcode(bytes: &[u8], mut i: usize, opsize: u8) -> Option<(u8, Shape, usize)> {
    let opcode = *bytes.get(i)?;
    i += 1;
    let (size, shape) = match opcode {
        0x88 => (1, Shape::Store),
        0x89 => (opsize, Shape::Store),
        0x8A => (
            1,
            Shape::Load {
                dest: 1,
                sign: false,
            },
        ),
        0x8B => (
            opsize,
            Shape::Load {
                dest: opsize,
                sign: false,
            },
        ),
        0xC6 => (1, Shape::Imm { imm_len: 1 }),
        0xC7 => (
            opsize,
            Shape::Imm {
                imm_len: if opsize == 2 { 2 } else { 4 },
            },
        ),
        0x0F => {
            let op2 = *bytes.get(i)?;
            i += 1;
            match op2 {
                0xB6 => (
                    1,
                    Shape::Load {
                        dest: opsize,
                        sign: false,
                    },
                ),
                0xB7 => (
                    2,
                    Shape::Load {
                        dest: opsize,
                        sign: false,
                    },
                ),
                0xBE => (
                    1,
                    Shape::Load {
                        dest: opsize,
                        sign: true,
                    },
                ),
                0xBF => (
                    2,
                    Shape::Load {
                        dest: opsize,
                        sign: true,
                    },
                ),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some((size, shape, i))
}

/// Walk the `ModRM` byte (+ `SIB` + displacement) at `bytes[i]` of a memory
/// operand. Returns the index just past the addressing bytes and the `ModRM`
/// `reg` field, or `None` for a register-direct operand (not a memory access)
/// or a truncated encoding.
fn parse_modrm(bytes: &[u8], mut i: usize) -> Option<(usize, u8)> {
    let modrm = *bytes.get(i)?;
    i += 1;
    let md = modrm >> 6;
    let rm = modrm & 7;
    if md == 3 {
        return None;
    }
    let mut disp = 0usize;
    if rm == 4 {
        let sib = *bytes.get(i)?;
        i += 1;
        if md == 0 && sib & 7 == 5 {
            disp = 4; // no base: disp32
        }
    } else if md == 0 && rm == 5 {
        disp = 4; // RIP-relative
    }
    disp = match md {
        1 => 1,
        2 => 4,
        _ => disp,
    };
    Some((i + disp, (modrm >> 3) & 7))
}

/// Read an `imm_len`-byte immediate at `bytes[i]`; a 32-bit immediate under
/// `REX.W` is sign-extended to 64 bits. Returns it and the index past it.
fn parse_imm(bytes: &[u8], i: usize, imm_len: usize, rex_w: bool) -> Option<(u64, usize)> {
    let raw = bytes.get(i..i + imm_len)?;
    let mut buf = [0u8; 8];
    buf[..imm_len].copy_from_slice(raw);
    let value = if rex_w && imm_len == 4 {
        i64::from(i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])).cast_unsigned()
    } else {
        u64::from_le_bytes(buf)
    };
    Some((value, i + imm_len))
}

/// Decode the instruction at the start of `bytes` (long mode), or `None` if it
/// is not one of the supported forms or `bytes` ends mid-instruction.
#[must_use]
pub fn decode_mem_access(bytes: &[u8]) -> Option<MemAccess> {
    let (pfx, i) = parse_prefixes(bytes);
    let opcode_at = i;
    let (size, shape, i) = parse_opcode(bytes, i, pfx.operand_size())?;
    let (i, reg_field) = parse_modrm(bytes, i)?;
    let reg = reg_field | pfx.rex_r();
    // Without REX, byte registers 4-7 are AH/CH/DH/BH, not SPL/BPL/SIL/DIL.
    let byte_regs_high = pfx.rex == 0 && (4..8).contains(&reg);
    let is_plain_byte_load = bytes.get(opcode_at) == Some(&0x8A);
    let (kind, len) = match shape {
        Shape::Store => (
            AccessKind::Store {
                reg,
                high8: size == 1 && byte_regs_high,
            },
            i,
        ),
        Shape::Load { dest, sign } => (
            AccessKind::Load {
                reg,
                // Only the plain 8-bit mov (8A) can name AH/CH/DH/BH; movzx and
                // movsx write a full-size register.
                high8: is_plain_byte_load && byte_regs_high,
                sign_extend: sign,
                dest_size: dest,
            },
            i,
        ),
        Shape::Imm { imm_len } => {
            if reg_field != 0 {
                return None; // C6/C7 are only `mov` for /0
            }
            let (value, end) = parse_imm(bytes, i, imm_len, pfx.rex_w())?;
            (AccessKind::StoreImm { value }, end)
        }
    };
    Some(MemAccess { len, size, kind })
}

/// Mask `value` to `size` bytes.
#[must_use]
pub const fn mask_to(value: u64, size: u8) -> u64 {
    match size {
        1 => value & 0xFF,
        2 => value & 0xFFFF,
        4 => value & 0xFFFF_FFFF,
        _ => value,
    }
}

/// Extend a `size`-byte `value` to a register of `dest_size` bytes.
#[must_use]
pub const fn extend(value: u64, size: u8, dest_size: u8, sign: bool) -> u64 {
    let v = mask_to(value, size);
    let v = if sign && size < 8 && v & (1 << (size * 8 - 1)) != 0 {
        v | !((1u64 << (size * 8)) - 1)
    } else {
        v
    };
    mask_to(v, dest_size)
}

/// Merge `value` into the old register contents `old` as a register write does.
///
/// 8/16-bit writes preserve the upper bits, a 32-bit write zero-extends, a
/// 64-bit write replaces. `high8` targets bits 15:8.
#[must_use]
pub const fn merge_reg(old: u64, value: u64, dest_size: u8, high8: bool) -> u64 {
    match dest_size {
        1 if high8 => (old & !0xFF00) | ((value & 0xFF) << 8),
        1 => (old & !0xFF) | (value & 0xFF),
        2 => (old & !0xFFFF) | (value & 0xFFFF),
        4 => value & 0xFFFF_FFFF,
        _ => value,
    }
}

/// Translate guest-virtual `gva` to guest-physical via the guest's page tables.
///
/// Walks the 4-level tables rooted at `cr3`. `read_u64` reads a guest-physical
/// 8-byte table entry (`None` if it is unreadable). Returns `None` when the
/// address is not mapped.
pub fn translate_gva(cr3: u64, gva: u64, read_u64: &impl Fn(u64) -> Option<u64>) -> Option<u64> {
    const PRESENT: u64 = 1;
    const PS: u64 = 1 << 7;
    const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
    let idx = |shift: u32| (gva >> shift) & 0x1FF;

    let pml4e = read_u64((cr3 & ADDR) + idx(39) * 8)?;
    if pml4e & PRESENT == 0 {
        return None;
    }
    let pdpte = read_u64((pml4e & ADDR) + idx(30) * 8)?;
    if pdpte & PRESENT == 0 {
        return None;
    }
    if pdpte & PS != 0 {
        return Some((pdpte & 0x000F_FFFF_C000_0000) | (gva & 0x3FFF_FFFF));
    }
    let pde = read_u64((pdpte & ADDR) + idx(21) * 8)?;
    if pde & PRESENT == 0 {
        return None;
    }
    if pde & PS != 0 {
        return Some((pde & 0x000F_FFFF_FFE0_0000) | (gva & 0x1F_FFFF));
    }
    let pte = read_u64((pde & ADDR) + idx(12) * 8)?;
    if pte & PRESENT == 0 {
        return None;
    }
    Some((pte & ADDR) | (gva & 0xFFF))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(b: &[u8]) -> MemAccess {
        decode_mem_access(b).unwrap_or_else(|| panic!("failed to decode {b:02x?}"))
    }

    #[test]
    fn decodes_a_32bit_load_through_a_pointer() {
        // mov (%rdi),%eax
        let m = d(&[0x8B, 0x07]);
        assert_eq!(m.len, 2);
        assert_eq!(m.size, 4);
        assert_eq!(
            m.kind,
            AccessKind::Load {
                reg: 0,
                high8: false,
                sign_extend: false,
                dest_size: 4
            }
        );
    }

    #[test]
    fn decodes_64bit_and_16bit_operand_sizes() {
        // mov (%rdi),%rax  /  mov (%rdi),%ax
        let q = d(&[0x48, 0x8B, 0x07]);
        assert_eq!((q.len, q.size), (3, 8));
        let w = d(&[0x66, 0x8B, 0x07]);
        assert_eq!((w.len, w.size), (3, 2));
    }

    #[test]
    fn decodes_stores_and_extended_registers() {
        // mov %r9d,(%rdi)  (REX.R selects r9)
        let m = d(&[0x44, 0x89, 0x0F]);
        assert_eq!(m.len, 3);
        assert_eq!(m.size, 4);
        assert_eq!(
            m.kind,
            AccessKind::Store {
                reg: 9,
                high8: false
            }
        );
        // movb %cl,(%rdi)
        let b = d(&[0x88, 0x0F]);
        assert_eq!((b.len, b.size), (2, 1));
        assert_eq!(
            b.kind,
            AccessKind::Store {
                reg: 1,
                high8: false
            }
        );
    }

    #[test]
    fn decodes_displacements_sib_and_rip_relative() {
        // mov 0x10(%rdi),%eax  -> modrm 47, disp8
        assert_eq!(d(&[0x8B, 0x47, 0x10]).len, 3);
        // mov 0x100(%rdi),%eax -> modrm 87, disp32
        assert_eq!(d(&[0x8B, 0x87, 0x00, 0x01, 0x00, 0x00]).len, 6);
        // mov (%rdi,%rax,4),%eax -> modrm 04, sib 87
        assert_eq!(d(&[0x8B, 0x04, 0x87]).len, 3);
        // mov 0x0(,%rax,4),%eax -> modrm 04, sib 85 (no base) + disp32
        assert_eq!(d(&[0x8B, 0x04, 0x85, 0, 0, 0, 0]).len, 7);
        // mov disp32(%rip),%eax -> modrm 05
        assert_eq!(d(&[0x8B, 0x05, 0x10, 0, 0, 0]).len, 6);
    }

    #[test]
    fn decodes_store_immediates() {
        // movl $0x12345678,(%rdi)
        let m = d(&[0xC7, 0x07, 0x78, 0x56, 0x34, 0x12]);
        assert_eq!((m.len, m.size), (6, 4));
        assert_eq!(m.kind, AccessKind::StoreImm { value: 0x1234_5678 });
        // movq $-1,(%rdi): sign-extended imm32 under REX.W
        let q = d(&[0x48, 0xC7, 0x07, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!((q.len, q.size), (7, 8));
        assert_eq!(q.kind, AccessKind::StoreImm { value: u64::MAX });
        // movb $0x7f,(%rdi)
        let b = d(&[0xC6, 0x07, 0x7F]);
        assert_eq!((b.len, b.size), (3, 1));
        assert_eq!(b.kind, AccessKind::StoreImm { value: 0x7F });
        // movw $0x1234,(%rdi)
        let w = d(&[0x66, 0xC7, 0x07, 0x34, 0x12]);
        assert_eq!((w.len, w.size), (5, 2));
        assert_eq!(w.kind, AccessKind::StoreImm { value: 0x1234 });
    }

    #[test]
    fn decodes_zero_and_sign_extending_loads() {
        // movzbl (%rdi),%eax  /  movzwl (%rdi),%eax
        let z = d(&[0x0F, 0xB6, 0x07]);
        assert_eq!((z.len, z.size), (3, 1));
        assert_eq!(
            z.kind,
            AccessKind::Load {
                reg: 0,
                high8: false,
                sign_extend: false,
                dest_size: 4
            }
        );
        assert_eq!(d(&[0x0F, 0xB7, 0x07]).size, 2);
        // movsbq (%rdi),%rax
        let s = d(&[0x48, 0x0F, 0xBE, 0x07]);
        assert_eq!((s.len, s.size), (4, 1));
        assert_eq!(
            s.kind,
            AccessKind::Load {
                reg: 0,
                high8: false,
                sign_extend: true,
                dest_size: 8
            }
        );
    }

    #[test]
    fn high_byte_registers_need_no_rex() {
        // mov (%rdi),%ah (8A 27): reg field 4 without REX is AH
        let m = d(&[0x8A, 0x27]);
        assert_eq!(
            m.kind,
            AccessKind::Load {
                reg: 4,
                high8: true,
                sign_extend: false,
                dest_size: 1
            }
        );
        // With a REX prefix reg 4 is SPL, not AH.
        let r = d(&[0x40, 0x8A, 0x27]);
        assert_eq!(
            r.kind,
            AccessKind::Load {
                reg: 4,
                high8: false,
                sign_extend: false,
                dest_size: 1
            }
        );
    }

    #[test]
    fn rejects_register_direct_unknown_and_truncated_instructions() {
        assert_eq!(
            decode_mem_access(&[0x89, 0xC8]),
            None,
            "mov %ecx,%eax is not memory"
        );
        assert_eq!(decode_mem_access(&[0x90]), None, "nop");
        assert_eq!(
            decode_mem_access(&[0xC7, 0x07, 0x78]),
            None,
            "truncated immediate"
        );
        assert_eq!(decode_mem_access(&[0x8B]), None, "no modrm");
        assert_eq!(decode_mem_access(&[]), None);
        assert_eq!(
            decode_mem_access(&[0xC7, 0x0F, 1, 0, 0, 0]),
            None,
            "C7 /1 is not mov"
        );
    }

    #[test]
    fn extend_and_merge_follow_x86_register_rules() {
        assert_eq!(extend(0xFF, 1, 4, false), 0xFF);
        assert_eq!(extend(0xFF, 1, 4, true), 0xFFFF_FFFF);
        assert_eq!(extend(0xFF, 1, 8, true), u64::MAX);
        assert_eq!(extend(0x8000, 2, 8, true), 0xFFFF_FFFF_FFFF_8000);
        assert_eq!(extend(0xFFFF_FFFF, 4, 8, false), 0xFFFF_FFFF);
        // 32-bit writes zero-extend; 8/16-bit writes merge.
        assert_eq!(merge_reg(u64::MAX, 0x11, 4, false), 0x11);
        assert_eq!(merge_reg(u64::MAX, 0x1122, 2, false), 0xFFFF_FFFF_FFFF_1122);
        assert_eq!(merge_reg(u64::MAX, 0x33, 1, false), 0xFFFF_FFFF_FFFF_FF33);
        assert_eq!(merge_reg(u64::MAX, 0x33, 1, true), 0xFFFF_FFFF_FFFF_33FF);
        assert_eq!(merge_reg(0, u64::MAX, 8, false), u64::MAX);
    }

    /// A tiny guest address space: page tables as a map of GPA -> entry.
    fn walk(gva: u64, tables: &[(u64, u64)]) -> Option<u64> {
        translate_gva(0x1000, gva, &|gpa| {
            tables.iter().find(|(a, _)| *a == gpa).map(|(_, e)| *e)
        })
    }

    #[test]
    fn walks_4k_2m_and_1g_mappings() {
        let ptr = |gpa: u64| gpa | 1;
        // gva 0x4020_1234: PML4[0] -> PDPT@0x2000, PDPT[1] -> PD@0x3000,
        // PD[1] -> PT@0x4000, PT[1] -> page 0x9000.
        let t = [
            (0x1000, ptr(0x2000)),
            (0x2000 + 8, ptr(0x3000)),
            (0x3000 + 8, ptr(0x4000)),
            (0x4000 + 8, ptr(0x9000)),
        ];
        assert_eq!(walk(0x4020_1234, &t), Some(0x9234));
        // 2 MiB page: PD[1] is a huge leaf at 0x80_0000.
        let t2 = [
            (0x1000, ptr(0x2000)),
            (0x2000 + 8, ptr(0x3000)),
            (0x3000 + 8, 0x80_0000 | 1 | (1 << 7)),
        ];
        assert_eq!(walk(0x4020_1234, &t2), Some(0x80_1234));
        // 1 GiB page: PDPT[1] is a huge leaf at 0x4000_0000.
        let t3 = [
            (0x1000, ptr(0x2000)),
            (0x2000 + 8, 0x4000_0000 | 1 | (1 << 7)),
        ];
        assert_eq!(walk(0x4020_1234, &t3), Some(0x4020_1234));
    }

    #[test]
    fn walk_fails_on_a_missing_level() {
        let ptr = |gpa: u64| gpa | 1;
        assert_eq!(walk(0x4020_1234, &[]), None, "unreadable PML4");
        assert_eq!(
            walk(0x4020_1234, &[(0x1000, 0)]),
            None,
            "PML4 entry not present"
        );
        let t = [
            (0x1000, ptr(0x2000)),
            (0x2000 + 8, ptr(0x3000)),
            (0x3000 + 8, 0),
        ];
        assert_eq!(walk(0x4020_1234, &t), None, "PDE not present");
    }
}

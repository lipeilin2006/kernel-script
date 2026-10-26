/// What the decoder reports about one instruction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Decoded {
    /// Instruction length in bytes.
    pub len: usize,
    /// Absolute offset (from the instruction start) of a RIP-relative
    /// disp32 operand that must be fixed up when the instruction is copied
    /// elsewhere.
    pub rip_disp: Option<usize>,
    /// The instruction is a relative branch (`jmp`/`jcc`/`call`/`loop`),
    /// so it cannot be copied verbatim.
    pub relative_branch: bool,
}

/// Decodes the instruction at the start of `code`. Returns `None` for
/// anything the conservative table does not understand: a wrong length would
/// split an instruction.
pub(crate) fn decode(code: &[u8]) -> Option<Decoded> {
    let mut index = 0usize;
    let mut rex_w = false;
    let mut opsize = false;
    let mut addrsize = false;

    // Legacy prefixes, then at most one REX.
    loop {
        let byte = *code.get(index)?;
        match byte {
            0x66 => opsize = true,
            0x67 => addrsize = true,
            0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {}
            _ => break,
        }
        index += 1;
        if index > 14 {
            return None;
        }
    }
    if let Some(&byte) = code.get(index) {
        if (0x40..=0x4F).contains(&byte) {
            rex_w = byte & 0x08 != 0;
            index += 1;
        }
    }

    let opcode = *code.get(index)?;
    index += 1;

    let mut decoded = Decoded::default();

    let length = match opcode {
        0x0F => two_byte(code, &mut index, &mut decoded, rex_w, opsize, addrsize),
        0xC4 | 0xC5 | 0x62 => None, // VEX / EVEX: unsupported on purpose
        0x9A | 0xEA => None,        // far call/jmp, invalid in long mode
        0xE8 | 0xE9 => {
            decoded.relative_branch = true;
            Some(index + 4)
        }
        0xEB | 0x70..=0x7F | 0xE0..=0xE3 => {
            decoded.relative_branch = true;
            Some(index + 1)
        }
        0xA0..=0xA3 => Some(index + if addrsize { 4 } else { 8 }),
        0xA8 | 0xCD | 0xD4 | 0xD5 | 0xE4..=0xE7 => Some(index + 1),
        0xA9 => Some(index + imm_16_32(opsize)),
        0xB0..=0xB7 => Some(index + 1),
        0xB8..=0xBF => Some(index + if rex_w { 8 } else { imm_16_32(opsize) }),
        0xC2 | 0xCA => Some(index + 2),
        0xC8 => Some(index + 3),
        0x68 => Some(index + imm_16_32(opsize)),
        0x6A => Some(index + 1),
        0x69 | 0x6B => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            let immediate = if opcode == 0x6B { 1 } else { imm_16_32(opsize) };
            Some(index + modrm.len + immediate)
        }
        0x80 | 0x82 | 0x83 => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            Some(index + modrm.len + 1)
        }
        0x81 => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            Some(index + modrm.len + imm_16_32(opsize))
        }
        0xC0 | 0xC1 => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            Some(index + modrm.len + 1)
        }
        0xC6 => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            Some(index + modrm.len + 1)
        }
        0xC7 => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            Some(index + modrm.len + imm_16_32(opsize))
        }
        0xF6 | 0xF7 => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            let immediate = if modrm.reg <= 1 {
                if opcode == 0xF6 {
                    1
                } else {
                    imm_16_32(opsize)
                }
            } else {
                0
            };
            Some(index + modrm.len + immediate)
        }
        0x00..=0x3F => {
            if (opcode & 0x07) > 3 {
                // Segment push/pop, daa/das, aaa/aas, and the segment
                // prefix slots when they appear as an opcode: no operands.
                Some(index)
            } else {
                let modrm = modrm_len(&code[index..], addrsize)?;
                note_modrm(&mut decoded, index, &modrm);
                Some(index + modrm.len)
            }
        }
        0x50..=0x5F => Some(index), // push/pop r64
        0x63 | 0x84..=0x8F | 0xD0..=0xD3 | 0xD8..=0xDF | 0xFE | 0xFF => {
            let modrm = modrm_len(&code[index..], addrsize)?;
            note_modrm(&mut decoded, index, &modrm);
            Some(index + modrm.len)
        }
        // Remaining single-byte forms take no ModRM and no immediate.
        0x60 | 0x61 => None,
        0x90..=0x99
        | 0x9B..=0x9F
        | 0xC3
        | 0xC9
        | 0xCB
        | 0xCC
        | 0xCE
        | 0xCF
        | 0xD7
        | 0xEC..=0xEF
        | 0xF1
        | 0xF4
        | 0xF5
        | 0xF8..=0xFD => Some(index),
        _ => None,
    }?;

    // A length is only usable when the whole instruction is present in the
    // buffer; otherwise the caller could believe in bytes it never read.
    if code.len() < length || length > 15 {
        return None;
    }
    decoded.len = length;
    Some(decoded)
}

/// Length-only view of [`decode`].
pub(crate) fn instruction_len(code: &[u8]) -> Option<usize> {
    decode(code).map(|decoded| decoded.len)
}

fn note_modrm(decoded: &mut Decoded, index: usize, modrm: &ModrmInfo) {
    if let Some(relative) = modrm.rip_disp {
        decoded.rip_disp = Some(index + relative);
    }
}

fn imm_16_32(opsize: bool) -> usize {
    if opsize {
        2
    } else {
        4
    }
}

fn two_byte(
    code: &[u8],
    index: &mut usize,
    decoded: &mut Decoded,
    rex_w: bool,
    opsize: bool,
    addrsize: bool,
) -> Option<usize> {
    let opcode = *code.get(*index)?;
    *index += 1;
    match opcode {
        0x05
        | 0x06
        | 0x07
        | 0x08
        | 0x09
        | 0x0B
        | 0x0E
        | 0x30..=0x37
        | 0x77
        | 0xA0..=0xA2
        | 0xA8
        | 0xA9
        | 0xAA
        | 0xC8..=0xCF => Some(*index),
        0x80..=0x8F => {
            decoded.relative_branch = true;
            Some(*index + 4)
        }
        0x38 => None, // 3-byte map: immediate sizes vary
        0x3A => {
            let modrm = modrm_len(&code[*index..], addrsize)?;
            note_modrm(decoded, *index, &modrm);
            Some(*index + modrm.len + 1)
        }
        0x0F => {
            let modrm = modrm_len(&code[*index..], addrsize)?;
            note_modrm(decoded, *index, &modrm);
            Some(*index + modrm.len + 1)
        }
        0x70 | 0x71 | 0x72 | 0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4 | 0xC5 | 0xC6 => {
            let modrm = modrm_len(&code[*index..], addrsize)?;
            note_modrm(decoded, *index, &modrm);
            Some(*index + modrm.len + 1)
        }
        0x0D
        | 0x10..=0x17
        | 0x18..=0x1F
        | 0x28..=0x2F
        | 0x40..=0x4F
        | 0x50..=0x6F
        | 0x74..=0x76
        | 0x78
        | 0x79
        | 0x7C..=0x7F
        | 0x90..=0x9F
        | 0xA3
        | 0xA5
        | 0xAB
        | 0xAD
        | 0xAE
        | 0xAF
        | 0xB0..=0xB9
        | 0xBB..=0xC1
        | 0xC3
        | 0xC7
        | 0xD0..=0xFF => {
            let modrm = modrm_len(&code[*index..], addrsize)?;
            note_modrm(decoded, *index, &modrm);
            Some(*index + modrm.len)
        }
        _ => {
            let _ = (rex_w, opsize);
            None
        }
    }
}

/// Decoded ModRM/SIB/displacement bytes.
struct ModrmInfo {
    /// Total operand length (ModRM + SIB + displacement).
    len: usize,
    /// ModRM.reg field.
    reg: u8,
    /// Offset of a RIP-relative disp32 within the operand, if any.
    rip_disp: Option<usize>,
}

/// Decodes ModRM/SIB/displacement bytes.
fn modrm_len(code: &[u8], addrsize: bool) -> Option<ModrmInfo> {
    let modrm = *code.first()?;
    let mode = modrm >> 6;
    let reg = (modrm >> 3) & 0x07;
    let rm = modrm & 0x07;
    let mut length = 1usize;
    let mut rip_disp = None;

    if mode != 0b11 {
        let needs_sib = rm == 0b100;
        if needs_sib {
            let sib = *code.get(length)?;
            length += 1;
            if mode == 0 && (sib & 0x07) == 0b101 {
                length += 4;
            }
        } else if mode == 0 && rm == 0b101 {
            // RIP-relative unless the address-size prefix selected 32-bit
            // addressing, where the same encoding is absolute disp32.
            if !addrsize {
                rip_disp = Some(length);
            }
            length += 4;
        }
        match mode {
            0b01 => length += 1,
            0b10 => length += 4,
            _ => {}
        }
    }
    Some(ModrmInfo {
        len: length,
        reg,
        rip_disp,
    })
}

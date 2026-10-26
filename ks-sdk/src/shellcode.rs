//! Client-side shellcode assembly for the kernel detour API.
//!
//! The driver only relocates the stolen prologue and lays out the executable
//! buffer (see `ks_sdk::hook_detour`); the detour bytes themselves are built
//! here, on the client, where they can be unit-tested. Only fixed encodings
//! are supported — this is deliberately not a general assembler.

/// x64 general-purpose registers usable with [`ShellcodeBuilder::push_reg`]
/// and [`ShellcodeBuilder::pop_reg`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reg {
    Rax,
    Rcx,
    Rdx,
    Rbx,
    Rsp,
    Rbp,
    Rsi,
    Rdi,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
}

impl Reg {
    /// Encoding used by the `push r64` / `pop r64` opcodes.
    fn code(self) -> u8 {
        match self {
            Self::Rax => 0,
            Self::Rcx => 1,
            Self::Rdx => 2,
            Self::Rbx => 3,
            Self::Rsp => 4,
            Self::Rbp => 5,
            Self::Rsi => 6,
            Self::Rdi => 7,
            Self::R8 => 8,
            Self::R9 => 9,
            Self::R10 => 10,
            Self::R11 => 11,
            Self::R12 => 12,
            Self::R13 => 13,
            Self::R14 => 14,
            Self::R15 => 15,
        }
    }
}

/// `call qword ptr [rip-14]`: calls the driver-built trampoline that replays
/// the original prologue. Valid only inside a `hook_detour` code buffer.
pub const CALL_ORIGINAL: [u8; 6] = [0xFF, 0x15, 0xF2, 0xFF, 0xFF, 0xFF];

/// Accumulates machine code bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellcodeBuilder {
    code: Vec<u8>,
}

impl ShellcodeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends raw bytes (for instructions this builder does not model).
    pub fn bytes(&mut self, raw: &[u8]) -> &mut Self {
        self.code.extend_from_slice(raw);
        self
    }

    /// `push r64`.
    pub fn push_reg(&mut self, reg: Reg) -> &mut Self {
        self.push_pop(0x50, reg)
    }

    /// `pop r64`.
    pub fn pop_reg(&mut self, reg: Reg) -> &mut Self {
        self.push_pop(0x58, reg)
    }

    /// Pushes the given registers in order.
    pub fn push_regs(&mut self, regs: &[Reg]) -> &mut Self {
        for reg in regs {
            self.push_reg(*reg);
        }
        self
    }

    /// Pops the given registers in reverse order (stack discipline).
    pub fn pop_regs(&mut self, regs: &[Reg]) -> &mut Self {
        for reg in regs.iter().rev() {
            self.pop_reg(*reg);
        }
        self
    }

    fn push_pop(&mut self, opcode: u8, reg: Reg) -> &mut Self {
        let code = reg.code();
        if code >= 8 {
            self.code.push(0x41); // REX.B
            self.code.push(opcode + (code - 8));
        } else {
            self.code.push(opcode + code);
        }
        self
    }

    /// `call qword ptr [rip-14]` — the original function via the driver's
    /// trampoline.
    pub fn call_original(&mut self) -> &mut Self {
        self.bytes(&CALL_ORIGINAL)
    }

    /// `call qword ptr [rip+0]` followed by the absolute address.
    pub fn call_absolute(&mut self, address: u64) -> &mut Self {
        self.indirect(0x15, address)
    }

    /// `jmp qword ptr [rip+0]` followed by the absolute address.
    pub fn jmp_absolute(&mut self, address: u64) -> &mut Self {
        self.indirect(0x25, address)
    }

    fn indirect(&mut self, opcode: u8, address: u64) -> &mut Self {
        self.code
            .extend_from_slice(&[0xFF, opcode, 0x00, 0x00, 0x00, 0x00]);
        self.code.extend_from_slice(&address.to_le_bytes());
        self
    }

    /// `mov rax, imm64`.
    pub fn mov_rax_imm64(&mut self, value: u64) -> &mut Self {
        self.code.extend_from_slice(&[0x48, 0xB8]);
        self.code.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// `add rax, imm8`.
    pub fn add_rax_imm8(&mut self, value: i8) -> &mut Self {
        self.code
            .extend_from_slice(&[0x48, 0x83, 0xC0, value as u8]);
        self
    }

    /// `ret`.
    pub fn ret(&mut self) -> &mut Self {
        self.code.push(0xC3);
        self
    }

    /// `int3`.
    pub fn int3(&mut self) -> &mut Self {
        self.code.push(0xCC);
        self
    }

    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// Returns the assembled bytes.
    pub fn build(&self) -> Vec<u8> {
        self.code.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_encodings() {
        let mut builder = ShellcodeBuilder::new();
        builder.push_reg(Reg::Rax).pop_reg(Reg::Rax);
        assert_eq!(builder.build(), [0x50, 0x58]);

        let mut builder = ShellcodeBuilder::new();
        builder.push_reg(Reg::R15).pop_reg(Reg::R15);
        assert_eq!(builder.build(), [0x41, 0x57, 0x41, 0x5F]);
    }

    #[test]
    fn push_regs_and_pop_regs_reverse() {
        let mut builder = ShellcodeBuilder::new();
        builder
            .push_regs(&[Reg::Rax, Reg::Rcx])
            .pop_regs(&[Reg::Rax, Reg::Rcx]);
        assert_eq!(builder.build(), [0x50, 0x51, 0x59, 0x58]);
    }

    #[test]
    fn indirect_forms_embed_the_address() {
        let mut builder = ShellcodeBuilder::new();
        builder.call_original();
        assert_eq!(builder.build(), CALL_ORIGINAL);

        let mut builder = ShellcodeBuilder::new();
        builder.call_absolute(0x1122_3344_5566_7788);
        let mut expected = vec![0xFF, 0x15, 0, 0, 0, 0];
        expected.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(builder.build(), expected);
    }

    #[test]
    fn detour_shape_matches_the_driver_self_test() {
        // The driver's detour self test uses exactly this sequence.
        let mut builder = ShellcodeBuilder::new();
        builder.call_original().add_rax_imm8(1).ret();
        assert_eq!(
            builder.build(),
            [0xFF, 0x15, 0xF2, 0xFF, 0xFF, 0xFF, 0x48, 0x83, 0xC0, 0x01, 0xC3]
        );
    }
}

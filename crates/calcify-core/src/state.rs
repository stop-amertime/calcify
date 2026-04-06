//! Emulator state — flat representation of the x86CSS machine.
//!
//! Replaces CSS's triple-buffered custom properties with direct mutable state.

/// CPU register indices into `State::registers`.
pub mod reg {
    pub const AX: usize = 0;
    pub const CX: usize = 1;
    pub const DX: usize = 2;
    pub const BX: usize = 3;
    pub const SP: usize = 4;
    pub const BP: usize = 5;
    pub const SI: usize = 6;
    pub const DI: usize = 7;
    pub const IP: usize = 8;
    pub const ES: usize = 9;
    pub const CS: usize = 10;
    pub const SS: usize = 11;
    pub const DS: usize = 12;
    pub const FLAGS: usize = 13;
    pub const COUNT: usize = 14;
}

/// Default memory size for x86CSS (0x600 bytes = 1,536).
pub const DEFAULT_MEM_SIZE: usize = 0x600;

/// The flat machine state that replaces CSS's triple-buffered custom properties.
#[derive(Debug, Clone)]
pub struct State {
    pub registers: [i32; reg::COUNT],
    pub memory: Vec<u8>,
    pub text_buffer: String,
    pub keyboard: u8,
    pub frame_counter: u32,
}

impl State {
    /// Create a new state with the given memory size.
    pub fn new(mem_size: usize) -> Self {
        Self {
            registers: [0; reg::COUNT],
            memory: vec![0; mem_size],
            text_buffer: String::new(),
            keyboard: 0,
            frame_counter: 0,
        }
    }

    /// Read a byte from the unified address space.
    ///
    /// Negative addresses map to registers (the same convention x86CSS uses):
    /// -1 = AX, -2 = CX, ..., -14 = flags.
    pub fn read_mem(&self, addr: i32) -> i32 {
        if addr < 0 {
            let reg_idx = (-addr - 1) as usize;
            if reg_idx < reg::COUNT {
                return self.registers[reg_idx];
            }
            0
        } else {
            let addr = addr as usize;
            if addr < self.memory.len() {
                self.memory[addr] as i32
            } else {
                0
            }
        }
    }

    /// Read a 16-bit little-endian word from memory.
    pub fn read_mem16(&self, addr: i32) -> i32 {
        let lo = self.read_mem(addr);
        let hi = self.read_mem(addr + 1);
        lo + hi * 256
    }

    /// Write a value to the unified address space.
    pub fn write_mem(&mut self, addr: i32, value: i32) {
        if addr < 0 {
            let reg_idx = (-addr - 1) as usize;
            if reg_idx < reg::COUNT {
                self.registers[reg_idx] = value;
            }
        } else {
            let addr = addr as usize;
            if addr < self.memory.len() {
                self.memory[addr] = (value & 0xFF) as u8;
            }
        }
    }

    /// Get the low byte of a 16-bit register value.
    pub fn lo8(value: i32) -> i32 {
        value & 0xFF
    }

    /// Get the high byte of a 16-bit register value.
    pub fn hi8(value: i32) -> i32 {
        (value >> 8) & 0xFF
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new(DEFAULT_MEM_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_addressing() {
        let mut state = State::default();
        state.registers[reg::AX] = 0x1234;
        assert_eq!(state.read_mem(-1), 0x1234);

        state.write_mem(-1, 0xABCD);
        assert_eq!(state.registers[reg::AX], 0xABCD);
    }

    #[test]
    fn memory_addressing() {
        let mut state = State::default();
        state.write_mem(0x100, 0x42);
        assert_eq!(state.read_mem(0x100), 0x42);
    }

    #[test]
    fn read16_little_endian() {
        let mut state = State::default();
        state.write_mem(0x10, 0x34); // lo
        state.write_mem(0x11, 0x12); // hi
        assert_eq!(state.read_mem16(0x10), 0x1234);
    }

    #[test]
    fn byte_extraction() {
        assert_eq!(State::lo8(0x1234), 0x34);
        assert_eq!(State::hi8(0x1234), 0x12);
    }
}

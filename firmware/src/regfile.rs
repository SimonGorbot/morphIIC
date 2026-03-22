use crate::model::{AUTO_INCREMENT, Access, DEFAULT_FILL, REG_COUNT, REGISTERS};

pub struct RegisterFile {
    regs: [u8; REG_COUNT],
    writable: [bool; REG_COUNT],
    pointer: u8,
}

impl RegisterFile {
    pub fn new() -> Self {
        let mut rf = Self {
            regs: [DEFAULT_FILL; REG_COUNT],
            writable: [false; REG_COUNT],
            pointer: 0,
        };
        rf.apply_model_defaults();
        rf
    }

    fn apply_model_defaults(&mut self) {
        for reg in REGISTERS {
            let idx = reg.addr as usize;
            self.regs[idx] = reg.default;
            self.writable[idx] = matches!(reg.access, Access::Rw);
        }
    }

    pub fn pointer(&self) -> u8 {
        self.pointer
    }

    pub fn set_pointer(&mut self, pointer: u8) {
        self.pointer = pointer;
    }

    pub fn write_payload(&mut self, payload: &[u8]) -> usize {
        let mut ptr = self.pointer;
        let mut accepted = 0usize;

        for byte in payload {
            let idx = ptr as usize;
            if self.writable[idx] {
                self.regs[idx] = *byte;
                accepted += 1;
            }

            if AUTO_INCREMENT {
                ptr = ptr.wrapping_add(1);
            }
        }

        self.pointer = ptr;
        accepted
    }

    pub fn read_into(&mut self, out: &mut [u8]) {
        let mut ptr = self.pointer;

        for byte in out {
            *byte = self.regs[ptr as usize];
            if AUTO_INCREMENT {
                ptr = ptr.wrapping_add(1);
            }
        }

        self.pointer = ptr;
    }

    pub fn rewind_pointer(&mut self, count: usize) {
        if AUTO_INCREMENT {
            self.pointer = self.pointer.wrapping_sub(count as u8);
        }
    }
}

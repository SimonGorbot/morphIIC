use crate::model::{AUTO_INCREMENT, Access, CSV_INDEX_BY_ADDR, DEFAULT_FILL, REG_COUNT, REGISTERS};
use crate::streams::{self, ReadEffect};

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

    pub fn reset_non_csv_to_defaults(&mut self) {
        for reg in REGISTERS {
            let idx = reg.addr as usize;
            if CSV_INDEX_BY_ADDR[idx] >= 0 {
                continue;
            }
            self.regs[idx] = reg.default;
        }
        self.pointer = 0;
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

    pub fn read_into(&mut self, out: &mut [u8], effects: &mut [ReadEffect]) {
        debug_assert!(effects.len() >= out.len());

        let mut ptr = self.pointer;

        for idx in 0..out.len() {
            let fallback = self.regs[ptr as usize];
            let (value, effect) = streams::read_for_register(ptr, fallback);
            out[idx] = value;
            effects[idx] = effect;

            if AUTO_INCREMENT {
                ptr = ptr.wrapping_add(1);
            }
        }

        self.pointer = ptr;
    }

    pub fn rollback_unread(&mut self, effects: &[ReadEffect], unread: usize) {
        if unread == 0 {
            return;
        }

        if AUTO_INCREMENT {
            self.pointer = self.pointer.wrapping_sub(unread as u8);
        }

        let rollback_start = effects.len().saturating_sub(unread);
        for effect in effects[rollback_start..].iter().rev() {
            streams::rollback_effect(*effect);
        }
    }
}

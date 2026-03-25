use core::cell::RefCell;

use critical_section::Mutex;

use crate::model::{
    CSV_INDEX_BY_ADDR, CSV_REGISTERS, CsvMode, HOST_STREAM_ADDRS, HOST_STREAM_BUFFER_CAPACITY,
    HOST_STREAM_COUNT, HOST_STREAM_INDEX_BY_ADDR, MAX_HOST_STREAM_REGS, REG_COUNT,
};

pub const MAX_STATUS_STREAMS: usize = MAX_HOST_STREAM_REGS;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
/// Describes any state change caused by reading a register stream value.
pub enum ReadEffect {
    /// Read had no stream-related side effects.
    None,
    /// Embedded CSV cursor advanced for the given register address.
    EmbeddedAdvance { addr: u8 },
    /// A host stream value was consumed and last-value state was updated.
    HostPop {
        stream_id: u8,
        value: u8,
        prev_has_last: bool,
        prev_last: u8,
    },
    /// Register was read while the host stream buffer was empty.
    HostUnderrun { stream_id: u8 },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FeedResult {
    pub accepted: usize,
    pub free: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
/// Errors that can occur while feeding bytes into a host-backed stream.
pub enum FeedError {
    InvalidStreamId,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HostStreamDescriptor {
    pub stream_id: u8,
    pub addr: u8,
    pub capacity: u16,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HostStreamStatus {
    pub stream_id: u8,
    pub addr: u8,
    pub level: usize,
    pub free: usize,
    pub underruns: u32,
    pub drops: u32,
    pub has_last: bool,
    pub last_value: u8,
}

#[derive(Copy, Clone)]
struct HostStreamState {
    addr: u8,
    buf: [u8; HOST_STREAM_BUFFER_CAPACITY],
    head: usize,
    len: usize,
    has_last: bool,
    last_value: u8,
    underruns: u32,
    drops: u32,
}

impl HostStreamState {
    /// Creates an empty stream state with placeholder address metadata.
    const fn empty() -> Self {
        Self {
            addr: 0xFF,
            buf: [0; HOST_STREAM_BUFFER_CAPACITY],
            head: 0,
            len: 0,
            has_last: false,
            last_value: 0,
            underruns: 0,
            drops: 0,
        }
    }

    /// Reinitializes stream contents and counters for a specific register address.
    fn reset_with_addr(&mut self, addr: u8) {
        self.addr = addr;
        self.head = 0;
        self.len = 0;
        self.has_last = false;
        self.last_value = 0;
        self.underruns = 0;
        self.drops = 0;
    }

    /// Pops and returns the oldest queued byte, if one is available.
    fn pop_front(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }

        let value = self.buf[self.head];
        self.head = (self.head + 1) % self.buf.len();
        self.len -= 1;
        Some(value)
    }

    /// Appends as much input data as possible and returns accepted byte count.
    fn push_back(&mut self, data: &[u8]) -> usize {
        let cap = self.buf.len();
        let free = cap - self.len;
        let accepted = data.len().min(free);

        let mut tail = (self.head + self.len) % cap;
        for value in &data[..accepted] {
            self.buf[tail] = *value;
            tail = (tail + 1) % cap;
        }

        self.len += accepted;
        accepted
    }

    /// Restores one byte to the front of the queue when capacity allows.
    fn push_front_one(&mut self, value: u8) {
        if self.len >= self.buf.len() {
            return;
        }
        self.head = if self.head == 0 {
            self.buf.len() - 1
        } else {
            self.head - 1
        };
        self.buf[self.head] = value;
        self.len += 1;
    }

    /// Returns the number of currently buffered bytes.
    fn level(&self) -> usize {
        self.len
    }

    /// Returns remaining queue capacity in bytes.
    fn free(&self) -> usize {
        self.buf.len() - self.len
    }
}

struct StreamManager {
    embedded_cursor: [u32; REG_COUNT],
    host: [HostStreamState; MAX_HOST_STREAM_REGS],
    host_count: usize,
}

impl StreamManager {
    /// Builds a stream manager with cleared cursors and empty host streams.
    const fn new() -> Self {
        Self {
            embedded_cursor: [0; REG_COUNT],
            host: [HostStreamState::empty(); MAX_HOST_STREAM_REGS],
            host_count: 0,
        }
    }

    /// Initializes stream routing from model tables and resets all stream state.
    fn init_from_model(&mut self) {
        self.embedded_cursor = [0; REG_COUNT];
        self.host_count = HOST_STREAM_COUNT.min(MAX_HOST_STREAM_REGS);

        for idx in 0..self.host_count {
            self.host[idx].reset_with_addr(HOST_STREAM_ADDRS[idx]);
        }
        for idx in self.host_count..self.host.len() {
            self.host[idx].reset_with_addr(0xFF);
        }
    }

    /// Clears cursors and buffered data while preserving configured host addresses.
    fn reset_all(&mut self) {
        self.embedded_cursor = [0; REG_COUNT];
        for idx in 0..self.host_count {
            let addr = self.host[idx].addr;
            self.host[idx].reset_with_addr(addr);
        }
    }

    /// Reads the next value for a register and records how state was affected.
    fn read_for_register(&mut self, addr: u8, fallback: u8) -> (u8, ReadEffect) {
        let csv_idx = CSV_INDEX_BY_ADDR[addr as usize];
        if csv_idx < 0 {
            return (fallback, ReadEffect::None);
        }

        let spec = &CSV_REGISTERS[csv_idx as usize];
        match spec.mode {
            CsvMode::Embedded => {
                if spec.data.is_empty() {
                    return (fallback, ReadEffect::None);
                }

                let len = spec.data.len() as u32;
                let cursor = self.embedded_cursor[addr as usize] % len;
                let value = spec.data[cursor as usize];
                self.embedded_cursor[addr as usize] = (cursor + 1) % len;

                (value, ReadEffect::EmbeddedAdvance { addr })
            }
            CsvMode::HostStream => {
                let host_idx = HOST_STREAM_INDEX_BY_ADDR[addr as usize];
                if host_idx < 0 {
                    return (fallback, ReadEffect::None);
                }

                let stream_id = host_idx as usize;
                let stream = &mut self.host[stream_id];

                if let Some(value) = stream.pop_front() {
                    let prev_has_last = stream.has_last;
                    let prev_last = stream.last_value;
                    stream.has_last = true;
                    stream.last_value = value;

                    return (
                        value,
                        ReadEffect::HostPop {
                            stream_id: stream_id as u8,
                            value,
                            prev_has_last,
                            prev_last,
                        },
                    );
                }

                stream.underruns = stream.underruns.wrapping_add(1);
                let value = if stream.has_last {
                    stream.last_value
                } else {
                    fallback
                };

                (
                    value,
                    ReadEffect::HostUnderrun {
                        stream_id: stream_id as u8,
                    },
                )
            }
        }
    }

    /// Reverts a previously returned `ReadEffect` to undo speculative reads.
    fn rollback_effect(&mut self, effect: ReadEffect) {
        match effect {
            ReadEffect::None => {}
            ReadEffect::EmbeddedAdvance { addr } => {
                let csv_idx = CSV_INDEX_BY_ADDR[addr as usize];
                if csv_idx < 0 {
                    return;
                }

                let spec = &CSV_REGISTERS[csv_idx as usize];
                if spec.data.is_empty() {
                    return;
                }

                let len = spec.data.len() as u32;
                let cursor = self.embedded_cursor[addr as usize] % len;
                self.embedded_cursor[addr as usize] =
                    if cursor == 0 { len - 1 } else { cursor - 1 };
            }
            ReadEffect::HostPop {
                stream_id,
                value,
                prev_has_last,
                prev_last,
            } => {
                let idx = stream_id as usize;
                if idx >= self.host_count {
                    return;
                }
                let stream = &mut self.host[idx];
                stream.push_front_one(value);
                stream.has_last = prev_has_last;
                stream.last_value = prev_last;
            }
            ReadEffect::HostUnderrun { stream_id } => {
                let idx = stream_id as usize;
                if idx >= self.host_count {
                    return;
                }
                let stream = &mut self.host[idx];
                stream.underruns = stream.underruns.saturating_sub(1);
            }
        }
    }

    /// Feeds host stream data and reports accepted bytes and remaining capacity.
    fn feed(&mut self, stream_id: u8, data: &[u8]) -> Result<FeedResult, FeedError> {
        let idx = stream_id as usize;
        if idx >= self.host_count {
            return Err(FeedError::InvalidStreamId);
        }

        let stream = &mut self.host[idx];
        let accepted = stream.push_back(data);
        let dropped = data.len().saturating_sub(accepted);
        if dropped > 0 {
            stream.drops = stream.drops.wrapping_add(dropped as u32);
        }

        Ok(FeedResult {
            accepted,
            free: stream.free(),
        })
    }

    /// Writes stream descriptors into `out` and returns number of entries written.
    fn descriptors(&self, out: &mut [HostStreamDescriptor]) -> usize {
        let count = self.host_count.min(out.len());
        for (idx, slot) in out.iter_mut().take(count).enumerate() {
            *slot = HostStreamDescriptor {
                stream_id: idx as u8,
                addr: self.host[idx].addr,
                capacity: HOST_STREAM_BUFFER_CAPACITY as u16,
            };
        }
        count
    }

    /// Writes current stream status snapshots into `out` and returns count written.
    fn statuses(&self, out: &mut [HostStreamStatus]) -> usize {
        let count = self.host_count.min(out.len());
        for (idx, slot) in out.iter_mut().take(count).enumerate() {
            let stream = &self.host[idx];
            *slot = HostStreamStatus {
                stream_id: idx as u8,
                addr: stream.addr,
                level: stream.level(),
                free: stream.free(),
                underruns: stream.underruns,
                drops: stream.drops,
                has_last: stream.has_last,
                last_value: stream.last_value,
            };
        }
        count
    }
}

static STREAM_MANAGER: Mutex<RefCell<StreamManager>> =
    Mutex::new(RefCell::new(StreamManager::new()));

/// Initializes stream state from static model definitions.
pub fn init() {
    critical_section::with(|cs| {
        STREAM_MANAGER.borrow(cs).borrow_mut().init_from_model();
    });
}

/// Clears all stream cursors, buffers, and runtime counters.
pub fn reset_all() {
    critical_section::with(|cs| {
        STREAM_MANAGER.borrow(cs).borrow_mut().reset_all();
    });
}

/// Retrieves the next value for a register along with its read side effect.
pub fn read_for_register(addr: u8, fallback: u8) -> (u8, ReadEffect) {
    critical_section::with(|cs| {
        STREAM_MANAGER
            .borrow(cs)
            .borrow_mut()
            .read_for_register(addr, fallback)
    })
}

/// Reverts stream state previously produced by `read_for_register`.
pub fn rollback_effect(effect: ReadEffect) {
    critical_section::with(|cs| {
        STREAM_MANAGER
            .borrow(cs)
            .borrow_mut()
            .rollback_effect(effect);
    });
}

/// Queues host-provided bytes for a stream selected by `stream_id`.
pub fn feed(stream_id: u8, data: &[u8]) -> Result<FeedResult, FeedError> {
    critical_section::with(|cs| STREAM_MANAGER.borrow(cs).borrow_mut().feed(stream_id, data))
}

/// Copies host stream descriptors into `out` and returns entries written.
pub fn descriptors(out: &mut [HostStreamDescriptor]) -> usize {
    critical_section::with(|cs| STREAM_MANAGER.borrow(cs).borrow().descriptors(out))
}

/// Copies current host stream status values into `out` and returns count written.
pub fn statuses(out: &mut [HostStreamStatus]) -> usize {
    critical_section::with(|cs| STREAM_MANAGER.borrow(cs).borrow().statuses(out))
}

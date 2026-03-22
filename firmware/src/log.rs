use core::cell::RefCell;
use core::fmt::Write as _;

use critical_section::Mutex;
use heapless::String;

pub const RING_CAPACITY: usize = 128;
const DATA_PREVIEW_BYTES: usize = 15;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EventKind {
    Write,
    WriteRead,
    Read,
    GeneralCall,
    ListenError,
    ReadError,
    Reset,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub seq: u32,
    pub kind: EventKind,
    pub pointer: u8,
    pub len: u16,
    pub status: u8,
    pub error: u32,
    pub data_len: u8,
    pub data: [u8; DATA_PREVIEW_BYTES],
}

impl Event {
    pub const fn empty() -> Self {
        Self {
            seq: 0,
            kind: EventKind::Read,
            pointer: 0,
            len: 0,
            status: 0,
            error: 0,
            data_len: 0,
            data: [0; DATA_PREVIEW_BYTES],
        }
    }
}

struct Ring {
    entries: [Event; RING_CAPACITY],
    next: usize,
    count: usize,
    seq: u32,
}

impl Ring {
    const fn new() -> Self {
        Self {
            entries: [Event::empty(); RING_CAPACITY],
            next: 0,
            count: 0,
            seq: 0,
        }
    }

    fn push(&mut self, mut event: Event) {
        self.seq = self.seq.wrapping_add(1);
        event.seq = self.seq;

        self.entries[self.next] = event;
        self.next = (self.next + 1) % RING_CAPACITY;
        if self.count < RING_CAPACITY {
            self.count += 1;
        }
    }

    fn clear(&mut self) {
        self.next = 0;
        self.count = 0;
        self.seq = 0;
    }
}

static LOG_RING: Mutex<RefCell<Ring>> = Mutex::new(RefCell::new(Ring::new()));

pub fn record(kind: EventKind, pointer: u8, len: usize, status: u8, error: u32, payload: &[u8]) {
    let mut data = [0u8; DATA_PREVIEW_BYTES];
    let copy_len = payload.len().min(DATA_PREVIEW_BYTES);
    data[..copy_len].copy_from_slice(&payload[..copy_len]);

    let event = Event {
        seq: 0,
        kind,
        pointer,
        len: len as u16,
        status,
        error,
        data_len: copy_len as u8,
        data,
    };

    critical_section::with(|cs| {
        LOG_RING.borrow(cs).borrow_mut().push(event);
    });
}

pub fn clear() {
    critical_section::with(|cs| {
        LOG_RING.borrow(cs).borrow_mut().clear();
    });
}

pub fn snapshot(out: &mut [Event]) -> usize {
    critical_section::with(|cs| {
        let ring = LOG_RING.borrow(cs).borrow();
        let count = ring.count.min(out.len());
        let start = (ring.next + RING_CAPACITY - count) % RING_CAPACITY;

        for (slot, offset) in out.iter_mut().zip(0..count) {
            let idx = (start + offset) % RING_CAPACITY;
            *slot = ring.entries[idx];
        }

        count
    })
}

pub fn format_event_line(event: &Event, line: &mut String<192>) {
    line.clear();

    let kind = match event.kind {
        EventKind::Write => "WRITE",
        EventKind::WriteRead => "WRRD",
        EventKind::Read => "READ",
        EventKind::GeneralCall => "GCALL",
        EventKind::ListenError => "LERR",
        EventKind::ReadError => "RERR",
        EventKind::Reset => "RESET",
    };

    let _ = write!(
        line,
        "#{:05} {} ptr=0x{:02X} len={} st={} err=0x{:08X} data=",
        event.seq, kind, event.pointer, event.len, event.status, event.error
    );

    if event.data_len == 0 {
        let _ = line.push_str("-");
        return;
    }

    for i in 0..(event.data_len as usize) {
        if i != 0 {
            let _ = line.push(' ');
        }
        let _ = write!(line, "{:02X}", event.data[i]);
    }
}

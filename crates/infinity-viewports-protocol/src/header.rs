use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::command::{COMMAND_RING_SLOTS, CommandSlot, ConfigSnapshot};
use crate::seqlock::Seqlock;
use crate::status::{LOG_RING_SLOTS, LogRecord, StatusSnapshot};
use crate::{PROTOCOL_VERSION_MAJOR, PROTOCOL_VERSION_MINOR, SHARED_MAGIC};

/// Fixed header at the start of the shared block.
#[repr(C)]
pub struct Header {
    /// `SHARED_MAGIC`. Lets the host reject an uninitialised or foreign block.
    pub magic: AtomicU32,
    pub version_major: AtomicU32,
    pub version_minor: AtomicU32,
    /// Size of the whole `SharedBlock`, as the bridge built it. The host checks
    /// this equals its own `size_of::<SharedBlock>()`.
    pub block_size: AtomicU32,
    /// Bridge process id (for diagnostics / name validation).
    pub bridge_pid: AtomicU32,
    pub _pad: AtomicU32,
    /// Set once by the bridge when the block is fully initialised and safe to
    /// read. The host spins on this (bounded) after opening the mapping.
    pub ready: AtomicU64,
}

impl Header {
    /// Bridge side: stamp the header. Call last, after the rest of the block is
    /// zeroed/initialised, so `ready` publishes a complete block.
    pub fn publish(&self, bridge_pid: u32, block_size: u32) {
        self.magic.store(SHARED_MAGIC, Ordering::Relaxed);
        self.version_major
            .store(PROTOCOL_VERSION_MAJOR as u32, Ordering::Relaxed);
        self.version_minor
            .store(PROTOCOL_VERSION_MINOR as u32, Ordering::Relaxed);
        self.block_size.store(block_size, Ordering::Relaxed);
        self.bridge_pid.store(bridge_pid, Ordering::Relaxed);
        self.ready.store(1, Ordering::Release);
    }

    /// Host side: validate magic, readiness and the major version. Returns the
    /// reason it is not usable, or `None` when it is.
    pub fn validate(&self, expected_block_size: u32) -> Option<HeaderReject> {
        if self.ready.load(Ordering::Acquire) == 0 {
            return Some(HeaderReject::NotReady);
        }
        if self.magic.load(Ordering::Relaxed) != SHARED_MAGIC {
            return Some(HeaderReject::BadMagic);
        }
        let major = self.version_major.load(Ordering::Relaxed);
        if major != PROTOCOL_VERSION_MAJOR as u32 {
            return Some(HeaderReject::MajorMismatch {
                bridge: major,
                host: PROTOCOL_VERSION_MAJOR as u32,
            });
        }
        let size = self.block_size.load(Ordering::Relaxed);
        if size != expected_block_size {
            return Some(HeaderReject::SizeMismatch {
                bridge: size,
                host: expected_block_size,
            });
        }
        None
    }

    /// Effective minor version both sides can use.
    pub fn negotiated_minor(&self) -> u32 {
        self.version_minor
            .load(Ordering::Relaxed)
            .min(PROTOCOL_VERSION_MINOR as u32)
    }
}

/// Why the host refused to drive a bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderReject {
    NotReady,
    BadMagic,
    MajorMismatch { bridge: u32, host: u32 },
    SizeMismatch { bridge: u32, host: u32 },
}

/// A seqlock-guarded config snapshot (header word immediately precedes payload).
#[repr(C)]
pub struct ConfigChannel {
    pub lock: Seqlock,
    pub snapshot: ConfigSnapshot,
}

/// A seqlock-guarded status snapshot.
#[repr(C)]
pub struct StatusChannel {
    pub lock: Seqlock,
    pub snapshot: StatusSnapshot,
}

/// Lossless host→bridge command ring. The host advances `head` after writing a
/// slot; the bridge advances `tail` after consuming one. Single producer,
/// single consumer.
#[repr(C)]
pub struct CommandRing {
    pub head: AtomicU64,
    pub tail: AtomicU64,
    pub slots: [CommandSlot; COMMAND_RING_SLOTS],
}

impl CommandRing {
    /// Host: push a command. Returns false if the ring is full (host should
    /// retry; the bridge drains every frame).
    pub fn push(&self, slot: CommandSlot) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= COMMAND_RING_SLOTS as u64 {
            return false;
        }
        let idx = (head as usize) % COMMAND_RING_SLOTS;
        // SAFETY: single producer; this index is not being read by the consumer
        // until `head` is advanced with Release below.
        unsafe {
            let p = self.slots.as_ptr().add(idx) as *mut CommandSlot;
            core::ptr::write_volatile(p, slot);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Bridge: pop the next command, if any.
    pub fn pop(&self) -> Option<CommandSlot> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail as usize) % COMMAND_RING_SLOTS;
        // SAFETY: single consumer; producer will not overwrite this slot until
        // `tail` is advanced.
        let slot = unsafe { core::ptr::read_volatile(self.slots.as_ptr().add(idx)) };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(slot)
    }
}

/// Bridge→host log ring (single producer, single consumer). Lossy under
/// overrun is acceptable; the host detects gaps by `LogRecord.seq`.
#[repr(C)]
pub struct LogRing {
    pub head: AtomicU64,
    pub tail: AtomicU64,
    pub slots: [LogRecord; LOG_RING_SLOTS],
}

impl LogRing {
    /// Bridge: append a record, dropping the oldest if the host is behind.
    pub fn push(&self, rec: LogRecord) {
        let head = self.head.load(Ordering::Relaxed);
        let idx = (head as usize) % LOG_RING_SLOTS;
        unsafe {
            let p = self.slots.as_ptr().add(idx) as *mut LogRecord;
            core::ptr::write_volatile(p, rec);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    /// Host: drain available records into `out`, returning how many were read.
    /// Records overwritten before the host read them are skipped (reflected as
    /// `seq` gaps).
    pub fn drain(&self, cursor: &mut u64, out: &mut [LogRecord]) -> usize {
        let head = self.head.load(Ordering::Acquire);
        // If we fell more than a ring behind, jump forward.
        let min_tail = head.saturating_sub(LOG_RING_SLOTS as u64);
        if *cursor < min_tail {
            *cursor = min_tail;
        }
        let mut n = 0;
        while *cursor < head && n < out.len() {
            let idx = (*cursor as usize) % LOG_RING_SLOTS;
            out[n] = unsafe { core::ptr::read_volatile(self.slots.as_ptr().add(idx)) };
            *cursor = cursor.wrapping_add(1);
            n += 1;
        }
        n
    }
}

/// The complete shared block. Both sides map this exact `#[repr(C)]` type.
#[repr(C)]
pub struct SharedBlock {
    pub header: Header,
    pub config: ConfigChannel,
    pub status: StatusChannel,
    pub commands: CommandRing,
    pub logs: LogRing,
}

impl SharedBlock {
    /// Size the host expects and the bridge reports in the header.
    pub const SIZE: u32 = core::mem::size_of::<SharedBlock>() as u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ring_roundtrip() {
        // Construct on the heap-equivalent (stack) to exercise the SPSC logic.
        let ring = CommandRing {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            slots: [CommandSlot::EMPTY; COMMAND_RING_SLOTS],
        };
        let mut s = CommandSlot::EMPTY;
        s.code = 7;
        assert!(ring.push(s));
        let got = ring.pop().unwrap();
        assert_eq!(got.code, 7);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn command_ring_full() {
        let ring = CommandRing {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            slots: [CommandSlot::EMPTY; COMMAND_RING_SLOTS],
        };
        for _ in 0..COMMAND_RING_SLOTS {
            assert!(ring.push(CommandSlot::EMPTY));
        }
        assert!(!ring.push(CommandSlot::EMPTY));
    }

    #[test]
    fn log_ring_gap_detection() {
        let ring = LogRing {
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            slots: [LogRecord::EMPTY; LOG_RING_SLOTS],
        };
        for i in 0..(LOG_RING_SLOTS as u64 + 5) {
            let mut r = LogRecord::EMPTY;
            r.seq = i;
            ring.push(r);
        }
        let mut cursor = 0;
        let mut out = [LogRecord::EMPTY; LOG_RING_SLOTS];
        let n = ring.drain(&mut cursor, &mut out);
        // We dropped the first 5; the oldest surviving seq is 5.
        assert_eq!(out[0].seq, 5);
        assert_eq!(n, LOG_RING_SLOTS);
    }
}

use core::sync::atomic::{AtomicU32, Ordering, fence};

#[repr(C)]
pub struct Seqlock {
    seq: AtomicU32,
}

impl Seqlock {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
        }
    }

    pub unsafe fn write<T>(&self, payload: *mut T, value: T) {
        let start = self.seq.load(Ordering::Relaxed);
        self.seq.store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        unsafe { core::ptr::write_volatile(payload, value) };
        fence(Ordering::Release);
        self.seq.store(start.wrapping_add(2), Ordering::Release);
    }

    pub unsafe fn read<T: Copy>(&self, payload: *const T, tries: u32) -> Option<T> {
        for _ in 0..tries {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            fence(Ordering::Acquire);
            let value = unsafe { core::ptr::read_volatile(payload) };
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return Some(value);
            }
            core::hint::spin_loop();
        }
        None
    }

    pub fn raw(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }
}

impl Default for Seqlock {
    fn default() -> Self {
        Self::new()
    }
}

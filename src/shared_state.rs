use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

pub type SharedStateRc = Rc<SharedState>;

/// State shared between bootloader and companion app
/// RTT sessions on the same core.
/// It's all single threaded, these act as counting semaphores.
#[derive(Debug)]
pub struct SharedState {
    clear_vector_catch_and_breakpoints: AtomicUsize,
    vector_catch_enabled: AtomicUsize,
}

impl SharedState {
    pub fn new_rc() -> SharedStateRc {
        Rc::new(Self {
            clear_vector_catch_and_breakpoints: AtomicUsize::new(0),
            vector_catch_enabled: AtomicUsize::new(0),
        })
    }

    pub fn reset(&self) {
        self.clear_vector_catch_and_breakpoints.store(0, SeqCst);
        self.vector_catch_enabled.store(0, SeqCst);
    }

    /// Returns true if this is the first to call
    pub fn inc_clear_vector_catch_and_breakpoints(&self) -> bool {
        incr_is_first(&self.clear_vector_catch_and_breakpoints)
    }

    /// Returns true if this is the last to call, or if enable was never called
    pub fn dec_clear_vector_catch_and_breakpoints(&self) -> bool {
        dec_is_last(&self.clear_vector_catch_and_breakpoints)
    }

    /// Returns true if this is the first to call
    pub fn inc_vector_catch_enabled(&self) -> bool {
        incr_is_first(&self.vector_catch_enabled)
    }

    /// Returns true if this is the last to call, or if enable was never called
    pub fn dec_vector_catch_enabled(&self) -> bool {
        dec_is_last(&self.vector_catch_enabled)
    }
}

fn incr_is_first(au: &AtomicUsize) -> bool {
    au.fetch_add(1, SeqCst) == 0
}

fn dec_is_last(au: &AtomicUsize) -> bool {
    let prev_val = au.fetch_sub(1, SeqCst);
    if prev_val == 0 {
        // Enable was never called, reset
        au.store(0, SeqCst);
    }
    prev_val == 1 || prev_val == 0
}

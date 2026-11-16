//! SCOPE 2.2 [N9]: SIGINT and SIGTERM stop the switch after the current package, as pacman's handler does; SIGHUP and SIGPIPE are ignored.
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use alpm::Alpm;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static COMMITTING: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

unsafe extern "C" {
    fn alpm_trans_interrupt(handle: *mut c_void) -> c_int;
}

extern "C" fn on_signal(_: c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
    let handle = COMMITTING.load(Ordering::SeqCst);
    if !handle.is_null() {
        unsafe { alpm_trans_interrupt(handle) };
    }
}

fn set(signal: c_int, handler: libc::sighandler_t) {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

/// Installs the switch's signal handling for the rest of the process.
pub fn install() {
    set(libc::SIGHUP, libc::SIG_IGN);
    set(libc::SIGPIPE, libc::SIG_IGN);
    let handler = on_signal as extern "C" fn(c_int) as libc::sighandler_t;
    set(libc::SIGINT, handler);
    set(libc::SIGTERM, handler);
}

/// Whether SIGINT or SIGTERM arrived since `install`.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// While alive, a signal interrupts `handle`'s commit after its current package.
pub struct Committing;

impl Committing {
    pub fn on(handle: &Alpm) -> Committing {
        COMMITTING.store(handle.as_alpm_handle_t().cast(), Ordering::SeqCst);
        Committing
    }
}

impl Drop for Committing {
    fn drop(&mut self) {
        COMMITTING.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}

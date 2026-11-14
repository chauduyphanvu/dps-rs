//! Ctrl-C, SIGTERM and SIGHUP while `dps process` runs: each long step checks
//! `check`, so the run stops at the next block or ffmpeg poll and its
//! temporary files are removed on the way out.

use std::sync::atomic::{AtomicI32, Ordering::Relaxed};

/// The signal that arrived, or 0.
static SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
pub fn install() -> Result<(), String> {
    use std::ffi::c_int;

    unsafe extern "C" {
        fn signal(signum: c_int, handler: usize) -> usize;
    }
    const SIG_DFL: usize = 0;
    const SIG_IGN: usize = 1;
    const SIG_ERR: usize = usize::MAX;

    extern "C" fn on_signal(signum: c_int) {
        SIGNAL.store(signum, Relaxed);
        // A second signal ends dps at once, in case stopping hangs.
        // SAFETY: signal() is async-signal-safe.
        unsafe { signal(signum, SIG_DFL) };
    }

    for signum in [1, 2, 15] {
        // SAFETY: the handler only stores an atomic and calls signal().
        let previous = unsafe { signal(signum, on_signal as extern "C" fn(c_int) as usize) };
        if previous == SIG_ERR {
            return Err("cannot install the Ctrl-C handler".into());
        }
        // A signal ignored when dps started, as under nohup, stays ignored.
        if previous == SIG_IGN {
            // SAFETY: SIG_IGN installs no code.
            unsafe { signal(signum, SIG_IGN) };
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn install() -> Result<(), String> {
    Ok(())
}

/// An error once a signal has arrived.
pub fn check() -> Result<(), String> {
    let name = match SIGNAL.load(Relaxed) {
        0 => return Ok(()),
        1 => "SIGHUP",
        2 => "Ctrl-C (SIGINT)",
        15 => "SIGTERM",
        _ => "a signal",
    };
    Err(format!(
        "stopped by {name}; nothing was written and the temporary files were removed"
    ))
}

#[cfg(test)]
pub fn raise(signum: i32) {
    SIGNAL.store(signum, Relaxed);
}

/// Held by tests that raise the signal or run what checks it, so one test's
/// signal does not stop another's run.
#[cfg(test)]
pub fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

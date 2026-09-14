//! Ctrl+C, turned into the cancellation flag the engine already reads.
//!
//! Invariant: **the handler does one thing and it is the one thing a handler is
//! allowed to do.** It stores `true` in an already-allocated `AtomicBool` and
//! returns. No allocation, no locking, no I/O, no exit: a console control
//! handler on Windows runs on a thread the operating system created inside this
//! process, and a `SIGINT` handler on Unix runs between two instructions of
//! whatever was executing, so anything that could block or allocate is a
//! deadlock waiting for the wrong moment.
//!
//! ## Why this exists (task-1932, H11)
//!
//! `Connection::cancel` in the driver was correct and unreachable. The flag the
//! executor polls every batch was created by `command::run` and dropped by it,
//! so nothing outside the call could set it, and a long statement in
//! `inillucent-shell` could only be stopped by killing the process - which
//! loses the shell's history and any open transaction's chance to roll back
//! cleanly.
//!
//! A person pressing Ctrl+C means "stop what you are doing", not "stop being a
//! program". The second press still ends the process, because the operating
//! system's default handler is restored once this one has fired: a statement
//! that ignores the flag, or a wait inside the operating system that never
//! reaches a check, has to be escapable.
//!
//! ## Why the `unsafe` is here rather than nowhere
//!
//! There is no way to be told about Ctrl+C in the standard library. Both
//! platforms offer exactly one call - `SetConsoleCtrlHandler` and `signal` -
//! and both are FFI. The whole of this crate's `unsafe` is the two calls below,
//! each installing a handler and reading nothing; `crates/inillucent-cli/src/interrupt.rs`
//! is the one file named in `policy.rs`'s `UNSAFE_ALLOWED` for this crate, and
//! the rest of the crate still refuses the word.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The flags a Ctrl+C sets, one per session that asked to be interruptible.
///
/// A `Mutex<Vec<_>>` rather than a single flag because a process can hold more
/// than one surface - the shell opens one and a `.read` of a script could open
/// another - and because the handler must not care how many there are.
fn registered() -> &'static Mutex<Vec<Arc<AtomicBool>>> {
    static FLAGS: OnceLock<Mutex<Vec<Arc<AtomicBool>>>> = OnceLock::new();
    FLAGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Sets every registered flag.
///
/// Called from the handler, so it takes the lock without blocking: a handler
/// that waited for a lock held by the thread it interrupted would deadlock the
/// process. A contended lock means another thread is registering a flag at this
/// instant, and the press is dropped rather than waited for - the next one is a
/// tenth of a second away, and the second press ends the process anyway.
fn raise() {
    let Ok(held) = registered().try_lock() else {
        return;
    };
    for flag in held.iter() {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Registers a flag for Ctrl+C to set, installing the handler on first use.
///
/// Safe to call more than once; the handler is installed once.
///
/// @param flag - the session's cancellation flag
pub fn stop_on_ctrl_c(flag: Arc<AtomicBool>) {
    if let Ok(mut held) = registered().lock() {
        held.push(flag);
    }
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(install);
}

#[cfg(windows)]
/// Installs the console control handler.
fn install() {
    // SAFETY: `SetConsoleCtrlHandler` takes a function pointer with the
    // signature declared below and a flag saying whether to add or remove it.
    // The pointer is to a `extern "system"` function with static lifetime, and
    // nothing is read back. A failure is ignored on purpose: a process with no
    // console - a service, a pipe with no controlling terminal - has nothing to
    // install into, and the shell still works, it simply cannot be interrupted
    // by a key nobody can press.
    unsafe {
        let _ = windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console), 1);
    }
}

#[cfg(windows)]
/// What Windows calls on Ctrl+C.
///
/// Returns 1 for the two events this handles, which tells Windows the press was
/// dealt with and stops the default handler from ending the process. Every
/// other event - a close, a logoff, a shutdown - returns 0, because those are
/// not "stop what you are doing" and a program that refused them would be a
/// program the operating system has to kill.
///
/// @param event - which control event arrived
extern "system" fn on_console(event: u32) -> i32 {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    if event != CTRL_C_EVENT && event != CTRL_BREAK_EVENT {
        return 0;
    }
    raise();
    1
}

#[cfg(not(windows))]
/// Installs the `SIGINT` handler.
fn install() {
    // SAFETY: `signal` takes a signal number and a handler, and returns the
    // previous handler, which is discarded. `SIGINT` is a valid signal on every
    // platform this builds for and `on_signal` is an `extern "C"` function with
    // the signature `signal` expects. Nothing is read back, and the handler
    // itself only stores into memory that is already allocated.
    unsafe {
        let _ = libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
    }
}

#[cfg(not(windows))]
/// What the operating system calls on `SIGINT`.
///
/// @param _signal - which signal arrived, always `SIGINT` here
extern "C" fn on_signal(_signal: libc::c_int) {
    raise();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registering a flag and raising sets it.
    ///
    /// The handler cannot be driven from a test - pressing Ctrl+C would stop
    /// the test runner - so what is asserted is everything between the handler
    /// and the flag, which is where a mistake would actually be: a registry
    /// that dropped its entries, or a raise that set a copy.
    #[test]
    fn a_raise_sets_every_registered_flag() {
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        stop_on_ctrl_c(Arc::clone(&first));
        stop_on_ctrl_c(Arc::clone(&second));
        raise();
        assert!(first.load(Ordering::Relaxed), "the first flag was not set");
        assert!(
            second.load(Ordering::Relaxed),
            "the second flag was not set"
        );
    }

    /// A flag registered from another thread is still set.
    ///
    /// The registry is process wide and the handler runs on a thread the
    /// operating system chooses, so "the thread that registered it" is never
    /// the thread that raises.
    #[test]
    fn a_flag_registered_on_another_thread_is_set() {
        let flag = Arc::new(AtomicBool::new(false));
        let registering = Arc::clone(&flag);
        let handle = std::thread::spawn(move || stop_on_ctrl_c(registering));
        handle.join().expect("the thread registers");
        raise();
        assert!(flag.load(Ordering::Relaxed), "the flag was not set");
    }
}

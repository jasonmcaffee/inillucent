//! The error handle, and the guard every entry point runs inside.
//!
//! Invariant: **no panic crosses the ABI.** Every entry point's body runs
//! inside `guarded` or `guarded_value`, which catch an unwind and turn it
//! into `INILLUCENT_INTERNAL` - because unwinding past an `extern "C"`
//! frame is undefined behaviour, and the caller is usually not Rust.

use std::ffi::{c_char, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use inillucent_driver::{Error, Status};

use crate::*;

/// One failure, with its strings already in C form.
pub struct inillucent_error {
    /// What kind of failure it was.
    status: i32,
    /// The safe message.
    message: CString,
    /// The construct the engine has not implemented, when that is why.
    feature: Option<CString>,
    /// The internal detail, when the caller asked for it.
    detail: Option<CString>,
    /// The byte offset into the statement, or -1.
    offset: i32,
}
/// Runs an entry point's body, turning a panic into a status.
///
/// **A panic unwinding into a C caller is undefined behaviour**, so every entry
/// point that answers a status wraps its body in this, and every entry point
/// that answers a value wraps its body in [`guarded_value`]. It used to say
/// "every entry point" and mean fifteen of fifty-three (task-1932, M9). It is a
/// backstop and not a strategy: the
/// driver denies `unwrap`, `expect`, `panic` and slice indexing, so a panic
/// reaching here is a defect, and [`INILLUCENT_INTERNAL`] is the status that
/// says "report this" rather than "you did something wrong".
///
/// @param entry - the entry point's name, for the message
/// @param error - where to put a failure, if the caller wants one
/// @param body - what to run
pub(crate) fn guarded<F>(entry: &str, error: *mut *mut inillucent_error, body: F) -> i32
where
    F: FnOnce() -> i32,
{
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(_) => {
            report(
                error,
                &Error::said(
                    Status::Internal,
                    format!("{entry} failed in a way it has no handling for; this is a defect."),
                ),
                false,
            );
            INILLUCENT_INTERNAL
        }
    }
}
/// Runs an entry point that answers a value rather than a status.
///
/// **The other thirty-eight (task-1932, M9).** The doc on [`guarded`] said
/// "every entry point wraps its body in this", and fifteen of the fifty-three
/// did. The rest could not: `guarded` answers an `i32` status and writes into
/// an error out-parameter, and these return a count, a pointer, a `f64` or
/// nothing at all, most of them with nowhere to put an error. A panic
/// unwinding out of any of them is undefined behaviour just the same, so they
/// needed a guard of their own rather than an exemption.
///
/// There is no error to report, so the fallback is each site's own "nothing"
/// answer - the same value it already returns for a null or dead handle, which
/// its doc comment already states. A caller that reads it is in exactly the
/// case it was already written for.
///
/// @param body - the entry point's work
/// @param fallback - what to answer if it panics
pub(crate) fn guarded_value<T, F>(body: F, fallback: T) -> T
where
    F: FnOnce() -> T,
{
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(fallback)
}
/// Writes a failure to the caller's out-parameter, if it wanted one.
///
/// A caller that passed NULL gets the status and nothing else, which is a
/// legitimate way to use the API for a call whose only interesting outcome is
/// whether it worked.
///
/// @param out - where the caller wants the failure, or null
/// @param failure - what went wrong
/// @param diagnostics - whether the internal detail may be carried
pub(crate) fn report(out: *mut *mut inillucent_error, failure: &Error, diagnostics: bool) {
    if out.is_null() {
        return;
    }
    let held = Box::new(inillucent_error {
        status: failure.status as i32,
        message: c_string(&failure.message),
        feature: failure.feature.as_deref().map(c_string),
        detail: match diagnostics {
            true => failure.detail.as_deref().map(c_string),
            false => None,
        },
        offset: failure
            .offset
            .map_or(-1, |at| i32::try_from(at).unwrap_or(-1)),
    });
    // SAFETY: `out` is a caller-supplied pointer the header requires to be
    // either null - checked above - or writable. Ownership of the box moves to
    // the caller, who frees it with `inillucent_error_free`.
    unsafe { *out = Box::into_raw(held) };
}
/// Builds a C string, replacing an interior NUL rather than failing.
///
/// A message with a NUL in it would otherwise be unreportable, and losing the
/// tail of an error message is a worse outcome than showing a `?` in it.
///
/// @param text - the text to convert
pub(crate) fn c_string(text: &str) -> CString {
    match CString::new(text) {
        Ok(built) => built,
        Err(_) => {
            let cleaned: String = text
                .chars()
                .map(|c| if c == '\0' { '?' } else { c })
                .collect();
            CString::new(cleaned).unwrap_or_else(|_| c"?".to_owned())
        }
    }
}
/// Reports the failure a null or spent handle deserves.
///
/// @param entry - the entry point's name
/// @param error - where the caller wants the failure
pub(crate) fn misused(entry: &str, error: *mut *mut inillucent_error) -> i32 {
    report(
        error,
        &Error::said(
            Status::InvalidState,
            format!("{entry} was given a handle that is null or already freed."),
        ),
        false,
    );
    INILLUCENT_INVALID_STATE
}
/// Turns a driver result into a status, reporting the failure if there is one.
///
/// @param outcome - what the driver answered
/// @param error - where the caller wants a failure
/// @param diagnostics - whether the internal detail may be carried
pub(crate) fn finish(
    outcome: inillucent_driver::Result<()>,
    error: *mut *mut inillucent_error,
    diagnostics: bool,
) -> i32 {
    match outcome {
        Ok(()) => INILLUCENT_OK,
        Err(why) => {
            let status = why.status as i32;
            report(error, &why, diagnostics);
            status
        }
    }
}
/// Returns what kind of failure it was.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_status(error: *const inillucent_error) -> i32 {
    guarded_value(
        || held(error).map_or(INILLUCENT_INVALID_STATE, |held| held.status),
        0,
    )
}
/// Returns what happened, in the engine's own words.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_message(error: *const inillucent_error) -> *const c_char {
    guarded_value(
        || match held(error) {
            Some(held) => held.message.as_ptr(),
            None => std::ptr::null(),
        },
        std::ptr::null(),
    )
}
/// Returns the construct the engine has not implemented, or null.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_feature(error: *const inillucent_error) -> *const c_char {
    guarded_value(
        || match held(error).and_then(|held| held.feature.as_ref()) {
            Some(feature) => feature.as_ptr(),
            None => std::ptr::null(),
        },
        std::ptr::null(),
    )
}
/// Returns the internal diagnostic text, or null.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_detail(error: *const inillucent_error) -> *const c_char {
    guarded_value(
        || match held(error).and_then(|held| held.detail.as_ref()) {
            Some(detail) => detail.as_ptr(),
            None => std::ptr::null(),
        },
        std::ptr::null(),
    )
}
/// Returns the byte offset into the statement, or -1.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_offset(error: *const inillucent_error) -> i32 {
    guarded_value(|| held(error).map_or(-1, |held| held.offset), 0)
}
/// Frees a failure.
///
/// @param error - the failure
///
/// # Safety
///
/// `error` must be null or a live handle, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn inillucent_error_free(error: *mut inillucent_error) {
    guarded_value(
        || {
            if !error.is_null() {
                drop(Box::from_raw(error));
            }
        },
        (),
    )
}

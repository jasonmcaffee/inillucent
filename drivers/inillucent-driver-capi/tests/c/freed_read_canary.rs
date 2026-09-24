//! A Rust side read of freed memory, built on purpose, so the sanitized
//! conformance run can prove it sees one before its own pass is believed.
//!
//! Invariant: `canary_read` after `canary_free` reads a box that `Box::from_raw`
//! has released. That is the pattern of the defect task-2098 fixed, where
//! `inillucent_path` read the liveness word of a database `inillucent_close` had
//! just freed. `conformance.rs` compiles this file on its own with `rustc`, not
//! as part of the crate, so nothing that ships contains it.

/// What the C side holds.
pub struct Canary {
    /// The word read after the box is freed.
    pub live: u64,
}

/// Allocates a canary and hands its pointer to C.
#[no_mangle]
pub extern "C" fn canary_new() -> *mut Canary {
    Box::into_raw(Box::new(Canary { live: 0x5eed }))
}

/// Releases a canary.
///
/// # Safety
/// `canary` came from `canary_new` and has not been freed.
///
/// @param canary - the pointer `canary_new` returned
#[no_mangle]
pub unsafe extern "C" fn canary_free(canary: *mut Canary) {
    drop(Box::from_raw(canary));
}

/// Reads the canary's word. Called after `canary_free`, this is the read of
/// freed memory the sanitizer has to report.
///
/// # Safety
/// None: the point is that it is unsafe.
///
/// @param canary - the pointer `canary_new` returned
#[no_mangle]
pub unsafe extern "C" fn canary_read(canary: *const Canary) -> u64 {
    std::ptr::read_volatile(&(*canary).live)
}

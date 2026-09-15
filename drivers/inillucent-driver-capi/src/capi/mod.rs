//! The C ABI's entry points, by the handle each is about.
//!
//! Invariant: **every entry point is re-exported from the crate root.** A
//! caller's Rust path is `inillucent_driver_capi::inillucent_open`, never
//! `inillucent_driver_capi::capi::db::inillucent_open`, so moving a function
//! between these modules is not a breaking change.

pub(crate) mod db;
pub(crate) mod error;
pub(crate) mod stmt;
pub(crate) mod value;

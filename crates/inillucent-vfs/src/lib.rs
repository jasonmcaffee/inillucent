//! The inillucent virtual file system.
//!
//! Invariant: this crate is the only part of the workspace that touches the
//! operating system's files, clocks, and randomness. Every layer above it -
//! pager, journal, WAL, catalog, VM - reaches persistent state through the
//! `Vfs` and `VfsFile` traits, which is what makes a deterministic simulator a
//! drop-in replacement for a disk rather than a parallel implementation.
//!
//! Three implementations ship here: `OsVfs` for the real file system,
//! `MemoryVfs` for in-process databases and fast tests, and - in `inillucent-sim` -
//! a simulator that adds failure and crash modelling on top of the same
//! contract. The `conformance` module holds one suite that all three must pass,
//! so "the simulator behaves like a disk" is a checked claim rather than a
//! hope.

#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod confine;
pub mod conformance;
pub mod contract;
pub mod error;
pub mod locks;
pub mod memory;
pub mod os;
pub mod path;
pub mod shm_locks;
pub mod zone;

pub use confine::{authorize, confine_process, process_root, Refused, Root};
pub use contract::{
    AccessMode, DeviceCharacteristics, FileIdentity, FileKind, FileLock, OpenOptions, SharedMemory,
    ShmLockRequest, ShmRegion, SyncMode, Vfs, VfsFile, SHM_LOCK_COUNT,
};
pub use error::{VfsError, VfsOperation, VfsResult};
pub use memory::MemoryVfs;
pub use os::OsVfs;
pub use path::DbPath;

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 1: VFS, binary primitives, and simulator";

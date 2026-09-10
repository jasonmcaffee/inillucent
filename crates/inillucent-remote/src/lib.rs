//! Reads a running PostgreSQL or MySQL server and migrates it into an
//! inillucent database, verified.
//!
//! ```no_run
//! # fn main() -> Result<(), inillucent_base::DbError> {
//! use inillucent_remote::{migrate, ConnectionUrl, Plan};
//!
//! let url = ConnectionUrl::parse("postgres://jason@127.0.0.1:5432/corpus")?;
//! let report = migrate::migrate(&Plan::new(url, "corpus.rdb"))?;
//! assert!(report.passed());
//! # Ok(())
//! # }
//! ```
//!
//! ## Why this crate exists, and why it has no dependencies
//!
//! `inillucent-migrate` reads *files* - a SQLite database and a legacy
//! retrieval index - and it links the whole retrieval engine to do the second
//! of those. That is why `inillucent migrate --kind index` refuses and points
//! at the other binary. If the remote sources lived there, `--kind postgres`
//! would have to refuse the same way, and the one thing this ticket needed was
//! for the new kinds to be reachable from the command line and from MCP.
//!
//! So this crate depends on `inillucent-base`, `inillucent-vfs`,
//! `inillucent-tree` and `inillucent-engine`, which is exactly what
//! `inillucent-cli` already links - and on **nothing else**. The `postgres`
//! crate is already in this workspace as a benchmark baseline and reaching for
//! it here would have been the short road, but it brings an async runtime into
//! a binary whose peak resident set is a published number, and `mysql` would be
//! a new third-party dependency on top. The protocols are documented, stable,
//! and small at the subset a reader needs; the authentication primitives they
//! are specified in terms of are in [`auth`], each checked against a published
//! vector.
//!
//! ## What is here
//!
//! | module | what it is |
//! |---|---|
//! | [`url`] | one connection URL, two schemes, and a `Display` that redacts the password |
//! | [`auth`] | MD5, SHA-1, HMAC-SHA-256, PBKDF2-HMAC-SHA-256 and base64 |
//! | [`stream`] | the socket, and the bounded reading both protocols are written against |
//! | [`http`] | a verified `GET` to a file on disk, for `inillucent setup-embeddings` |
//! | [`archive`] | zip and gzipped tar, far enough to take one file out |
//! | [`tls`] | verified TLS, through the platform's own implementation |
//! | [`postgres`] | the version 3 frontend/backend protocol |
//! | [`mysql`] | the MySQL and MariaDB client protocol |
//! | [`source`] | what a source database looks like from here, and the type map |
//! | [`migrate`] | inventory, stage, copy, verify by count and digest, publish by rename |

// **`deny` rather than `forbid`, for one module.** `tls::windows` and
// `tls::unix` reach SChannel and OpenSSL, which is an FFI call and nothing
// else: no cryptography is implemented in this workspace, the certificate is
// handed to the platform and the platform's answer is acted on. `forbid` cannot
// be relaxed anywhere, and the alternative to relaxing it was a TLS
// implementation written here, which would be a far larger security surface
// than the plaintext migration this replaces. Every `unsafe` block in those two
// files carries its own SAFETY note, which
// `crates/inillucent-compat/tests/policy.rs` checks.
#![deny(unsafe_code)]
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

pub mod archive;
pub mod auth;
pub mod http;
pub mod migrate;
pub mod mysql;
pub mod postgres;
pub mod source;
pub mod stream;
pub mod tls;
pub mod url;

pub use migrate::{Check, Plan, Report, RowDigest, TableReport};
pub use mysql::MysqlSource;
pub use postgres::PostgresSource;
pub use source::{Kind, RemoteSource, SourceColumn, SourceTable};
pub use stream::Stream;
pub use url::{ConnectionUrl, Scheme, Transport};

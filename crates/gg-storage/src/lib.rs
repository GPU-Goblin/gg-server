//! Persistence for GPU Goblin: PostgreSQL is the source of truth (ADR-0004); artifacts live in a
//! content-addressed object store (ADR-0007).
//!
//! Query functions take `&mut PgConnection` so callers compose them inside one transaction. This
//! crate performs no scheduling decisions; see `gg-control` for the transactional workflows.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backup;
pub mod db;
pub mod error;
pub mod models;
pub mod objects;
pub mod queries;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use db::Db;
pub use error::StorageError;
pub use objects::{ObjectStore, StoredObject};
pub use sqlx::{PgConnection, PgPool, Postgres, Transaction};

/// Convert a database timestamp to protocol milliseconds.
pub fn to_millis(t: time::OffsetDateTime) -> gg_core::UnixMillis {
    gg_core::UnixMillis((t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Convert protocol milliseconds to a database timestamp.
pub fn from_millis(m: gg_core::UnixMillis) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp_nanos(m.0 as i128 * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

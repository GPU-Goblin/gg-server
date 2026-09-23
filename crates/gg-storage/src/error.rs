//! Storage errors.

/// Errors from the database or the object store.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// SQL error.
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    /// Migration error.
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// Object store I/O.
    #[error("object store: {0}")]
    Io(#[from] std::io::Error),
    /// Uploaded bytes do not match the announced hash.
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// Announced hash.
        expected: gg_core::Sha256Digest,
        /// Computed hash.
        actual: gg_core::Sha256Digest,
    },
    /// Object exceeds the declared or maximum size.
    #[error("object too large: {0} bytes > {1}")]
    TooLarge(u64, u64),
    /// Uploaded size differs from the announced size.
    #[error("size mismatch: expected {expected}, got {actual}")]
    SizeMismatch {
        /// Announced size.
        expected: u64,
        /// Received size.
        actual: u64,
    },
    /// Not found.
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// Invalid stored data (e.g. unknown state text).
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

impl StorageError {
    /// True when the underlying error is a unique-constraint violation.
    pub fn is_unique_violation(&self) -> bool {
        match self {
            StorageError::Sql(sqlx::Error::Database(e)) => e.is_unique_violation(),
            _ => false,
        }
    }

    /// True when the underlying error is a CHECK violation (e.g. negative budget).
    pub fn is_check_violation(&self) -> bool {
        match self {
            StorageError::Sql(sqlx::Error::Database(e)) => e.is_check_violation(),
            _ => false,
        }
    }

    /// Name of the violated constraint, if any.
    pub fn constraint(&self) -> Option<String> {
        match self {
            StorageError::Sql(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
            _ => None,
        }
    }
}

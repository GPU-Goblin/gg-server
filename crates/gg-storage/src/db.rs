//! Connection pool and migrations.

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};

use crate::StorageError;

/// Embedded migrations (`crates/gg-storage/migrations`).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Database handle.
#[derive(Clone, Debug)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connect with a bounded pool.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StorageError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply pending migrations.
    pub async fn migrate(&self) -> Result<(), StorageError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begin a transaction.
    pub async fn begin(&self) -> Result<Transaction<'static, Postgres>, StorageError> {
        Ok(self.pool.begin().await?)
    }

    /// Close the pool.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

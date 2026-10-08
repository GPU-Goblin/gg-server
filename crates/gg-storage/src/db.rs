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

/// A request cancelled while it opens a transaction (its client went away) can hand its connection
/// back to the pool inside that transaction: sqlx 0.9 sends `BEGIN`, and if the future is dropped
/// before it has counted the transaction, it does not roll it back. Every later use of that
/// connection would then write into a transaction nobody commits, while answering success. Before
/// a connection goes back to the idle pool, this asks the server whether it is inside a transaction
/// block and rolls it back if so.
///
/// The question costs one simple query: in the simple query protocol, a statement outside a
/// transaction block starts its own transaction, whose start time is exactly the statement's, so
/// `now() = statement_timestamp()`; inside an open block, `now()` is older.
async fn close_leaked_transaction(conn: &mut sqlx::PgConnection) -> Result<bool, sqlx::Error> {
    use sqlx::Row;
    let row = sqlx::raw_sql("SELECT now() = statement_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    let outside: bool = row.try_get(0)?;
    if !outside {
        tracing::warn!("a connection came back to the pool inside a transaction: rolled back");
        sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await?;
    }
    Ok(true)
}

impl Db {
    /// Connect with a bounded pool.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StorageError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .after_release(|conn, _meta| {
                Box::pin(async move { close_leaked_transaction(conn).await })
            })
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

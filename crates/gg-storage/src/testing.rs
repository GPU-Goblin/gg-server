//! Ephemeral PostgreSQL databases for integration tests.
//!
//! Requires `DATABASE_URL` (see `.env.example`). Each call creates `gg_test_<hex>` from the
//! maintenance connection, migrates it and returns a [`Db`]; the database is dropped on
//! [`TestDb::drop`] (best effort).

use crate::{Db, StorageError};

/// A test database handle.
pub struct TestDb {
    /// Connected and migrated handle.
    pub db: Db,
    name: String,
    admin_url: String,
}

fn base_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok().or_else(|| {
        // best effort: read .env at the workspace root
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
        let text = std::fs::read_to_string(root).ok()?;
        text.lines().find_map(|l| {
            l.strip_prefix("DATABASE_URL=")
                .map(|v| v.trim().trim_matches('"').to_owned())
        })
    })
}

impl TestDb {
    /// Create a fresh database, or return `None` if `DATABASE_URL` is not configured.
    pub async fn create() -> Result<Option<TestDb>, StorageError> {
        let Some(url) = base_url() else {
            return Ok(None);
        };
        let name = format!("gg_test_{}", uuid::Uuid::now_v7().simple());
        let admin = Db::connect(&url, 2).await?;
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(admin.pool())
            .await?;
        admin.close().await;
        let test_url = replace_db_name(&url, &name);
        let db = Db::connect(&test_url, 8).await?;
        db.migrate().await?;
        Ok(Some(TestDb {
            db,
            name,
            admin_url: url,
        }))
    }

    /// Same as [`TestDb::create`] but panics with a clear message if the database is unavailable.
    pub async fn create_or_panic() -> TestDb {
        match Self::create().await {
            Ok(Some(t)) => t,
            Ok(None) => panic!(
                "DATABASE_URL not set: start PostgreSQL with `cargo xtask db up` and copy .env.example to .env"
            ),
            Err(e) => panic!("cannot create test database: {e}"),
        }
    }

    /// Connection URL of this database.
    pub fn url(&self) -> String {
        replace_db_name(&self.admin_url, &self.name)
    }

    /// Drop the database (best effort).
    pub async fn drop(self) {
        self.db.close().await;
        if let Ok(admin) = Db::connect(&self.admin_url, 1).await {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            )))
            .execute(admin.pool())
            .await;
            admin.close().await;
        }
    }
}

fn replace_db_name(url: &str, name: &str) -> String {
    // postgres://user:pass@host:port/dbname[?params]
    let (head, tail) = match url.rfind('/') {
        Some(i) => (&url[..i], &url[i + 1..]),
        None => (url, ""),
    };
    let params = tail.find('?').map(|i| &tail[i..]).unwrap_or("");
    format!("{head}/{name}{params}")
}

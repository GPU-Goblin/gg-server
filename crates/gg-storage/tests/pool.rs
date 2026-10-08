//! A connection handed back to the pool inside a transaction (a request cancelled while it opened
//! one: sqlx 0.9 does not roll it back) must not swallow the next user's writes.

use gg_storage::Db;
use gg_storage::testing::TestDb;

#[tokio::test]
async fn a_transaction_left_open_is_rolled_back_before_the_connection_is_reused() {
    let t = TestDb::create_or_panic().await;
    // One connection: every acquire below gets the same one back.
    let one = Db::connect(&t.url(), 1).await.unwrap();
    {
        let mut c = one.pool().acquire().await.unwrap();
        sqlx::raw_sql("CREATE TABLE leak_probe (x int)")
            .execute(&mut *c)
            .await
            .unwrap();
    }
    {
        // What a cancelled `begin()` leaves behind: the server inside a transaction block that
        // sqlx does not know about.
        let mut c = one.pool().acquire().await.unwrap();
        sqlx::raw_sql("BEGIN").execute(&mut *c).await.unwrap();
    }
    {
        let mut c = one.pool().acquire().await.unwrap();
        sqlx::raw_sql("INSERT INTO leak_probe VALUES (1)")
            .execute(&mut *c)
            .await
            .unwrap();
    }
    // Another pool sees the write: it was committed, not held in the leaked transaction.
    let other = Db::connect(&t.url(), 1).await.unwrap();
    let mut c = other.pool().acquire().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM leak_probe")
        .fetch_one(&mut *c)
        .await
        .unwrap();
    assert_eq!(n, 1, "the write went into a transaction nobody commits");
    drop(c);
    one.close().await;
    other.close().await;
    t.drop().await;
}

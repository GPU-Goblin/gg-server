//! Transactions aborted by the database to resolve a deadlock: recognized as retryable, and
//! replayed by `workflows::retrying`. A real deadlock, not a simulated one: two transactions lock
//! two rows in opposite order, and PostgreSQL aborts one of them (40P01).

use gg_control::error::ApiError;
use gg_storage::testing::TestDb;

#[tokio::test]
async fn a_real_deadlock_is_recognised_as_retryable_and_replayed() {
    let tdb = TestDb::create_or_panic().await;
    let pool = tdb.db.pool();
    sqlx::query("CREATE TABLE locks_test (id int PRIMARY KEY)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO locks_test VALUES (1), (2)")
        .execute(pool)
        .await
        .unwrap();

    let mut t1 = pool.begin().await.unwrap();
    let mut t2 = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM locks_test WHERE id = 1 FOR UPDATE")
        .execute(&mut *t1)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM locks_test WHERE id = 2 FOR UPDATE")
        .execute(&mut *t2)
        .await
        .unwrap();
    // Each waits for the other's row: PostgreSQL aborts one of the two.
    let (a, b) = tokio::join!(
        sqlx::query("SELECT id FROM locks_test WHERE id = 2 FOR UPDATE").execute(&mut *t1),
        sqlx::query("SELECT id FROM locks_test WHERE id = 1 FOR UPDATE").execute(&mut *t2),
    );
    let aborted = match (a, b) {
        (Err(e), Ok(_)) | (Ok(_), Err(e)) => e,
        other => panic!("a deadlock was expected: {other:?}"),
    };
    let err = ApiError::from(aborted);
    assert!(err.is_retryable(), "{err}");
    drop((t1, t2));

    // Replayed: the first attempt fails as above, the second succeeds.
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let out = gg_control::workflows::retrying(|| async {
        if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Err(ApiError::Retryable("deadlock detected".into()))
        } else {
            Ok(42)
        }
    })
    .await
    .unwrap();
    assert_eq!(out, 42);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);

    // An error that is not a conflict is never replayed.
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let out: Result<(), ApiError> = gg_control::workflows::retrying(|| async {
        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ApiError::Conflict("lease expired".into()))
    })
    .await;
    assert!(out.is_err());
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

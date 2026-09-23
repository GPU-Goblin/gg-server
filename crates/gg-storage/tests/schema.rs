//! Schema-level invariants exercised against a real PostgreSQL.

use gg_core::*;
use gg_storage::queries::{identities, ledger};
use gg_storage::testing::TestDb;

#[tokio::test]
async fn migrations_apply_and_ledger_constraints_hold() {
    let t = TestDb::create_or_panic().await;
    let db = t.db.clone();
    let mut c = db.pool().acquire().await.unwrap();
    let p = ProjectId::new();
    identities::create_project(
        &mut c,
        p,
        "demo",
        ProjectState::Active,
        &["tensor.matmul.v1".into()],
    )
    .await
    .unwrap();
    let b = ledger::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (0, 0, 0));

    // negative budget is refused by CHECK
    let err = ledger::apply_budget_delta(&mut c, p, -1, 0, 0)
        .await
        .unwrap_err();
    assert!(err.is_check_violation(), "{err}");

    // unbalanced ledger transaction is refused at commit
    let mut tx = db.begin().await.unwrap();
    let txn = LedgerTxnId::new();
    ledger::insert_ledger_txn(&mut tx, txn, "grant", None, "test")
        .await
        .unwrap();
    ledger::insert_ledger_entry(&mut tx, txn, "system:grants", 5, 0)
        .await
        .unwrap();
    ledger::insert_ledger_entry(&mut tx, txn, "project:x:available", 0, 4)
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert!(err.to_string().contains("unbalanced"), "{err}");

    // balanced transaction commits; ledger is append-only
    let mut tx = db.begin().await.unwrap();
    let txn = LedgerTxnId::new();
    ledger::insert_ledger_txn(&mut tx, txn, "grant", None, "test")
        .await
        .unwrap();
    ledger::insert_ledger_entry(&mut tx, txn, "system:grants", 5, 0)
        .await
        .unwrap();
    ledger::insert_ledger_entry(&mut tx, txn, "project:x:available", 0, 5)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        ledger::account_balance(&mut c, "project:x:available")
            .await
            .unwrap(),
        5
    );
    let err = sqlx::query("DELETE FROM ledger_entries")
        .execute(db.pool())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"));
    let err = sqlx::query("UPDATE ledger_transactions SET kind = 'x'")
        .execute(db.pool())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"));

    // reward_key is unique
    let mut tx = db.begin().await.unwrap();
    ledger::insert_ledger_txn(&mut tx, LedgerTxnId::new(), "reward", Some("k1"), "")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    let err = ledger::insert_ledger_txn(&mut tx, LedgerTxnId::new(), "reward", Some("k1"), "")
        .await
        .unwrap_err();
    assert!(err.is_unique_violation());
    drop(tx);

    // single-use challenge
    let key = [7u8; 32];
    identities::insert_challenge(&mut c, &[1u8; 32], "worker-login", &key, 60)
        .await
        .unwrap();
    assert!(
        identities::consume_challenge(&mut c, &[1u8; 32], "worker-login", &key)
            .await
            .unwrap()
    );
    assert!(
        !identities::consume_challenge(&mut c, &[1u8; 32], "worker-login", &key)
            .await
            .unwrap()
    );
    assert!(
        !identities::consume_challenge(&mut c, &[2u8; 32], "worker-login", &key)
            .await
            .unwrap()
    );
    identities::insert_challenge(&mut c, &[3u8; 32], "validator-login", &key, 60)
        .await
        .unwrap();
    assert!(
        !identities::consume_challenge(&mut c, &[3u8; 32], "worker-login", &key)
            .await
            .unwrap(),
        "action bound"
    );
    drop(c);
    t.drop().await;
}

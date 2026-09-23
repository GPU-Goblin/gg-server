//! Ledger invariants against a real PostgreSQL: balanced entries, no double reservation under
//! concurrency, idempotent rewards.

use gg_core::*;
use gg_ledger::{LedgerError, accounts, commit_and_reward, grant, release, reserve, reward_key};
use gg_storage::Db;
use gg_storage::queries::{identities, jobs, ledger as lq, tasks as tq};
use gg_storage::testing::TestDb;

async fn setup(db: &Db) -> (ProjectId, WorkerId, TaskId) {
    let mut c = db.pool().acquire().await.unwrap();
    let p = ProjectId::new();
    identities::create_project(&mut c, p, "p", ProjectState::Active, &[])
        .await
        .unwrap();
    let w = WorkerId::new();
    identities::create_worker(&mut c, w, &[9u8; 32], "w")
        .await
        .unwrap();
    let e = ExperimentId::new();
    jobs::create_experiment(&mut c, e, p, "e", None)
        .await
        .unwrap();
    let j = JobId::new();
    jobs::create_job(&mut c, j, p, e, None, "j").await.unwrap();
    let t = TaskId::new();
    jobs::insert_task(
        &mut c,
        &jobs::NewTask {
            id: t,
            job_id: j,
            project_id: p,
            experiment_id: e,
            index: 0,
            compute_kind: "tensor.matmul.v1",
            spec_hash: Sha256Digest::zero(),
            definition_hash: Sha256Digest::zero(),
            spec_cbor: b"",
            max_attempts: 3,
            priority: 0,
            reward_primary: Credits(10),
            verification_cost: Credits(10),
            est_cpu_secs: 0.0,
            requirements: Default::default(),
        },
    )
    .await
    .unwrap();
    (p, w, t)
}

async fn new_attempt(db: &Db, t: TaskId, w: WorkerId, generation: i64) -> AttemptId {
    let mut c = db.pool().acquire().await.unwrap();
    let a = AttemptId::new();
    tq::insert_attempt(
        &mut c,
        a,
        t,
        w,
        generation,
        AttemptRole::Primary,
        b"",
        &Sha256Digest::zero(),
    )
    .await
    .unwrap();
    a
}

#[tokio::test]
async fn grant_reserve_commit_release_are_balanced() {
    let tdb = TestDb::create_or_panic().await;
    let db = tdb.db.clone();
    let (p, w, t) = setup(&db).await;

    let mut tx = db.begin().await.unwrap();
    grant(&mut tx, p, Credits(100), "test").await.unwrap();
    tx.commit().await.unwrap();

    let a1 = new_attempt(&db, t, w, 1).await;
    let mut tx = db.begin().await.unwrap();
    reserve(&mut tx, p, t, a1, Credits(20)).await.unwrap();
    tx.commit().await.unwrap();
    let mut c = db.pool().acquire().await.unwrap();
    let b = lq::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (80, 20, 0));

    // commit with reward 8 + verification 10 → remainder 2 back to available
    let mut tx = db.begin().await.unwrap();
    let post = commit_and_reward(
        &mut tx,
        a1,
        w,
        Credits(8),
        Credits(10),
        &reward_key(a1, AttemptRole::Primary),
    )
    .await
    .unwrap();
    assert!(post.created);
    tx.commit().await.unwrap();
    let b = lq::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (82, 0, 18));
    assert_eq!(
        lq::account_balance(&mut c, &accounts::earned(w))
            .await
            .unwrap(),
        8
    );
    assert_eq!(
        lq::account_balance(&mut c, accounts::VERIFICATION)
            .await
            .unwrap(),
        10
    );
    assert_eq!(
        lq::account_balance(&mut c, &accounts::available(p))
            .await
            .unwrap(),
        82
    );
    assert_eq!(
        lq::account_balance(&mut c, &accounts::reserved(p))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        lq::account_balance(&mut c, accounts::GRANTS).await.unwrap(),
        -100
    );
    assert!(
        lq::unbalanced_transactions(&mut c)
            .await
            .unwrap()
            .is_empty()
    );

    // release path
    let a2 = new_attempt(&db, t, w, 2).await;
    let mut tx = db.begin().await.unwrap();
    reserve(&mut tx, p, t, a2, Credits(30)).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    assert!(release(&mut tx, a2).await.unwrap());
    assert!(
        !release(&mut tx, a2).await.unwrap(),
        "second release is a no-op"
    );
    tx.commit().await.unwrap();
    let b = lq::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (82, 0, 18));
    // a released reservation cannot be committed
    let mut tx = db.begin().await.unwrap();
    let err = commit_and_reward(&mut tx, a2, w, Credits(1), Credits(1), "reward:other")
        .await
        .unwrap_err();
    assert!(matches!(err, LedgerError::ReservationState(_)));
    drop(tx);

    // insufficient budget fails without writing
    let a3 = new_attempt(&db, t, w, 3).await;
    let mut tx = db.begin().await.unwrap();
    let err = reserve(&mut tx, p, t, a3, Credits(1000)).await.unwrap_err();
    assert!(matches!(
        err,
        LedgerError::Insufficient {
            available: 82,
            required: 1000
        }
    ));
    drop(tx);
    let b = lq::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (82, 0, 18));
    drop(c);
    tdb.drop().await;
}

#[tokio::test]
async fn reward_is_idempotent_per_key() {
    let tdb = TestDb::create_or_panic().await;
    let db = tdb.db.clone();
    let (p, w, t) = setup(&db).await;
    let mut tx = db.begin().await.unwrap();
    grant(&mut tx, p, Credits(50), "t").await.unwrap();
    tx.commit().await.unwrap();
    let a = new_attempt(&db, t, w, 1).await;
    let mut tx = db.begin().await.unwrap();
    reserve(&mut tx, p, t, a, Credits(20)).await.unwrap();
    tx.commit().await.unwrap();
    let key = reward_key(a, AttemptRole::Primary);
    let mut tx = db.begin().await.unwrap();
    let first = commit_and_reward(&mut tx, a, w, Credits(10), Credits(10), &key)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // replay after commit: same txn, nothing written
    let mut tx = db.begin().await.unwrap();
    let second = commit_and_reward(&mut tx, a, w, Credits(10), Credits(10), &key)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(second.txn, first.txn);
    assert!(!second.created);
    let mut c = db.pool().acquire().await.unwrap();
    assert_eq!(
        lq::account_balance(&mut c, &accounts::earned(w))
            .await
            .unwrap(),
        10
    );
    let b = lq::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (30, 0, 20));
    drop(c);
    tdb.drop().await;
}

#[tokio::test]
async fn concurrent_reservations_never_exceed_budget() {
    let tdb = TestDb::create_or_panic().await;
    let db = tdb.db.clone();
    let (p, w, t) = setup(&db).await;
    let mut tx = db.begin().await.unwrap();
    grant(&mut tx, p, Credits(100), "t").await.unwrap();
    tx.commit().await.unwrap();

    // 20 concurrent transactions each try to reserve 15: at most 6 can succeed (90 ≤ 100 < 105).
    let mut handles = Vec::new();
    for i in 0..20i64 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let a = new_attempt(&db, t, w, i + 1).await;
            let mut tx = db.begin().await.unwrap();
            let r = reserve(&mut tx, p, t, a, Credits(15)).await;
            match r {
                Ok(_) => {
                    tx.commit().await.unwrap();
                    true
                }
                Err(LedgerError::Insufficient { .. }) => false,
                Err(e) => panic!("{e}"),
            }
        }));
    }
    let mut ok = 0;
    for h in handles {
        if h.await.unwrap() {
            ok += 1;
        }
    }
    assert_eq!(ok, 6);
    let mut c = db.pool().acquire().await.unwrap();
    let b = lq::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (10, 90, 0));
    assert_eq!(
        lq::account_balance(&mut c, &accounts::reserved(p))
            .await
            .unwrap(),
        90
    );
    assert!(
        lq::unbalanced_transactions(&mut c)
            .await
            .unwrap()
            .is_empty()
    );
    drop(c);
    tdb.drop().await;
}

#[tokio::test]
async fn concurrent_reward_replays_write_once() {
    let tdb = TestDb::create_or_panic().await;
    let db = tdb.db.clone();
    let (p, w, t) = setup(&db).await;
    let mut tx = db.begin().await.unwrap();
    grant(&mut tx, p, Credits(50), "t").await.unwrap();
    tx.commit().await.unwrap();
    let a = new_attempt(&db, t, w, 1).await;
    let mut tx = db.begin().await.unwrap();
    reserve(&mut tx, p, t, a, Credits(20)).await.unwrap();
    tx.commit().await.unwrap();
    let key = reward_key(a, AttemptRole::Primary);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = db.begin().await.unwrap();
            match commit_and_reward(&mut tx, a, w, Credits(10), Credits(10), &key).await {
                Ok(post) => {
                    // A concurrent writer may have inserted the key between our lookup and our
                    // insert: the unique constraint turns that into a failed commit, which the
                    // caller retries or treats as "already rewarded".
                    match tx.commit().await {
                        Ok(()) => Some(post.created),
                        Err(_) => None,
                    }
                }
                Err(LedgerError::ReservationState(_)) => None,
                Err(LedgerError::Storage(e)) if e.is_unique_violation() => None,
                Err(e) => panic!("{e}"),
            }
        }));
    }
    let mut created = 0;
    for h in handles {
        if h.await.unwrap() == Some(true) {
            created += 1;
        }
    }
    assert_eq!(created, 1, "exactly one effective reward write");
    let mut c = db.pool().acquire().await.unwrap();
    assert_eq!(
        lq::account_balance(&mut c, &accounts::earned(w))
            .await
            .unwrap(),
        10
    );
    let b = lq::get_budget(&mut c, p).await.unwrap().unwrap();
    assert_eq!((b.available, b.reserved, b.spent), (30, 0, 20));
    drop(c);
    tdb.drop().await;
}

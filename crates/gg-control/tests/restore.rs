//! Backup/restore: after a full run, dump the database and the object store, restore both into a
//! fresh database and directory, and check that receipts still verify and the ledger balances.

use gg_control::config::ServerConfig;
use gg_control::server::serve;
use gg_control::state::AppState;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::messages::ContributionReceiptV1;
use gg_protocol::signed::Signed;
use gg_storage::backup::{PgTools, copy_objects, dump_database, restore_database};
use gg_storage::queries::{identities, ledger as lq, tasks};
use gg_storage::testing::TestDb;
use gg_storage::{Db, ObjectStore};

#[tokio::test]
async fn dump_and_restore_preserve_receipts_and_ledger() {
    let tools =
        PgTools::detect().expect("pg_dump available on the host or in the docker container");
    let src = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("server");
    let cfg = ServerConfig {
        database_url: src.url(),
        data_dir: data_dir.clone(),
        network_id: "gg-restore".into(),
        ..ServerConfig::default()
    };
    let state = AppState::new(cfg, src.db.clone()).await.unwrap();
    let server_key = state.server_key().public_key();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = serve(state.clone(), listener).await.unwrap();

    // Produce data directly through the workflows: a project, a grant, an artifact, a receipt.
    let mut c = src.db.pool().acquire().await.unwrap();
    let p = ProjectId::new();
    identities::create_project(&mut c, p, "p", ProjectState::Active, &[])
        .await
        .unwrap();
    let mut tx = src.db.begin().await.unwrap();
    gg_ledger::grant(&mut tx, p, Credits(100), "restore-test")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let obj = state
        .objects()
        .put_bytes(b"artifact bytes", 1024)
        .await
        .unwrap();
    gg_storage::queries::artifacts::insert_artifact(
        &mut c,
        &obj.hash,
        obj.size,
        "application/octet-stream",
        "",
        "project",
        "",
        Some(p),
        "test",
    )
    .await
    .unwrap();
    let w = WorkerId::new();
    identities::create_worker(&mut c, w, &[3u8; 32], "w")
        .await
        .unwrap();
    let receipt = ContributionReceiptV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: NetworkId::new("gg-restore").unwrap(),
        receipt_id: ReceiptId::new(),
        project_id: p,
        experiment_id: ExperimentId::new(),
        job_id: JobId::new(),
        task_id: TaskId::new(),
        attempt_id: AttemptId::new(),
        role: AttemptRole::Primary,
        spec_hash: Sha256Digest::zero(),
        definition_hash: Sha256Digest::zero(),
        input_hashes: vec![obj.hash],
        submission_hash: Sha256Digest::zero(),
        output_hashes: vec![],
        decision_hash: Sha256Digest::zero(),
        decision_hashes: vec![Sha256Digest::zero()],
        worker_id: w,
        worker_public_key: gg_protocol::keys::PublicKey([3u8; 32]),
        beneficiary: None,
        amount: Credits(1),
        asset: "TC".into(),
        issued_at: UnixMillis::now(),
    };
    let signed = Signed::sign(
        state.server_key(),
        state.network(),
        Domain::Receipt,
        receipt,
    )
    .unwrap();
    let rhash = signed.body_hash(state.network(), Domain::Receipt).unwrap();
    let rcbor = signed.to_canonical_bytes().unwrap();
    let e = ExperimentId::new();
    gg_storage::queries::jobs::create_experiment(&mut c, e, p, "e", None)
        .await
        .unwrap();
    let j = JobId::new();
    gg_storage::queries::jobs::create_job(&mut c, j, p, e, None, "j")
        .await
        .unwrap();
    let t = TaskId::new();
    gg_storage::queries::jobs::insert_task(
        &mut c,
        &gg_storage::queries::jobs::NewTask {
            id: t,
            job_id: j,
            project_id: p,
            experiment_id: e,
            index: 0,
            compute_kind: "tensor.matmul.v1",
            spec_hash: Sha256Digest::zero(),
            definition_hash: Sha256Digest::zero(),
            spec_cbor: b"",
            max_attempts: 1,
            priority: 0,
            reward_primary: Credits(1),
            verification_cost: Credits(1),
            est_cpu_secs: 0.0,
            requirements: Default::default(),
        },
    )
    .await
    .unwrap();
    let a = AttemptId::new();
    tasks::insert_attempt(
        &mut c,
        a,
        t,
        w,
        1,
        AttemptRole::Primary,
        b"",
        &Sha256Digest::zero(),
    )
    .await
    .unwrap();
    tasks::insert_receipt(
        &mut c,
        signed.body.receipt_id,
        a,
        w,
        p,
        AttemptRole::Primary,
        Credits(1),
        None,
        &rhash,
        &rcbor,
    )
    .await
    .unwrap();
    drop(c);
    running.stop().await;

    // Backup.
    let dump = tmp.path().join("db.dump");
    let n = dump_database(&tools, &src.url(), &dump).unwrap();
    assert!(n > 0);
    let objects_backup = tmp.path().join("objects-backup");
    assert_eq!(
        copy_objects(&data_dir.join("objects"), &objects_backup).unwrap(),
        1
    );

    // Restore into a fresh database and directory.
    let dst = TestDb::create_or_panic().await;
    restore_database(&tools, &dst.url(), &dump).unwrap();
    let restored_dir = tmp.path().join("restored");
    copy_objects(&objects_backup, &restored_dir.join("objects")).unwrap();
    let store = ObjectStore::open(&restored_dir.join("objects")).unwrap();
    assert_eq!(
        store.read_bytes(&obj.hash, 1024).await.unwrap(),
        b"artifact bytes"
    );
    let db2 = Db::connect(&dst.url(), 2).await.unwrap();
    let mut c2 = db2.pool().acquire().await.unwrap();
    let rows = tasks::list_receipts_of_project(&mut c2, p, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let back: Signed<ContributionReceiptV1> =
        Signed::from_canonical_bytes(&rows[0].receipt_cbor).unwrap();
    back.verify(&NetworkId::new("gg-restore").unwrap(), Domain::Receipt)
        .unwrap();
    assert_eq!(back.signer, server_key);
    assert!(
        lq::unbalanced_transactions(&mut c2)
            .await
            .unwrap()
            .is_empty()
    );
    let b = lq::get_budget(&mut c2, p).await.unwrap().unwrap();
    assert_eq!(b.available, 100);
    // Ledger stays append-only after restore.
    let err = sqlx::query("DELETE FROM ledger_entries")
        .execute(db2.pool())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"));
    drop(c2);
    db2.close().await;
    dst.drop().await;
    src.drop().await;
}

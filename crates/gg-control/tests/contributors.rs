//! Contributor accounts (ADR-0030): anyone signs up with an e-mail code, with no admission; a
//! machine logged in with its key attaches itself with the account's enrollment code and is
//! authorized at once; suspending the account stops every machine of it.

use gg_control::config::ServerConfig;
use gg_control::server::{Running, serve};
use gg_control::state::AppState;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::LoginChallengeV1;
use gg_protocol::signed::Signed;
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;
use serde_json::{Value, json};

struct Env {
    tdb: Option<TestDb>,
    running: Option<Running>,
    http: reqwest::Client,
    base: String,
    admin: String,
    network: NetworkId,
    _tmp: tempfile::TempDir,
}

async fn start(signup: bool) -> Env {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-contrib".into(),
        dev_mode: true,
        contributor_signup: signup,
        ..ServerConfig::default()
    };
    cfg.email_login.provider = "simulated".into();
    cfg.validate().unwrap();
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = serve(state, listener).await.unwrap();
    let admin = gg_control::auth::new_token(gg_control::auth::API_TOKEN_PREFIX).unwrap();
    let mut c = tdb.db.pool().acquire().await.unwrap();
    identities::create_token(
        &mut c,
        &gg_control::auth::token_hash(&admin),
        "admin",
        None,
        "test",
        None,
    )
    .await
    .unwrap();
    drop(c);
    Env {
        base: running.url(),
        running: Some(running),
        tdb: Some(tdb),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        admin,
        network: NetworkId::new("gg-contrib").unwrap(),
        _tmp: tmp,
    }
}

impl Env {
    async fn stop(mut self) {
        if let Some(r) = self.running.take() {
            r.stop().await;
        }
        if let Some(t) = self.tdb.take() {
            t.drop().await;
        }
    }

    async fn api(&self, token: &str, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .http
            .post(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    async fn form(
        &self,
        path: &str,
        form: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> reqwest::Response {
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header("Sec-Fetch-Site", "same-origin")
            .form(form);
        if let Some(c) = cookie {
            req = req.header("cookie", format!("gg_contrib={c}"));
        }
        req.send().await.unwrap()
    }

    async fn page(&self, path: &str, cookie: Option<&str>) -> (u16, String) {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if let Some(c) = cookie {
            req = req.header("cookie", format!("gg_contrib={c}"));
        }
        let r = req.send().await.unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    /// The newest eight-digit code in the simulated mailbox.
    async fn newest_code(&self) -> String {
        let (_, mailbox) = self.page("/portal/dev/mailbox", None).await;
        let at = mailbox
            .find("verification code is:")
            .expect("a code in the mailbox");
        mailbox[at..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take(8)
            .collect()
    }

    /// Sign up (or in) and return the session cookie.
    async fn sign_up(&self, email: &str) -> String {
        assert_eq!(
            self.form("/portal/contribute/email", &[("email", email)], None)
                .await
                .status(),
            200
        );
        let code = self.newest_code().await;
        let r = self
            .form(
                "/portal/contribute/verify",
                &[("email", email), ("code", &code)],
                None,
            )
            .await;
        assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
        cookie_value(&r, "gg_contrib").expect("session cookie")
    }

    async fn new_enrollment_code(&self, session: &str) -> String {
        let r = self
            .form("/portal/contribute/token", &[], Some(session))
            .await;
        assert_eq!(r.status(), 200);
        let page = r.text().await.unwrap();
        let at = page.find("gge_").expect("an enrollment code on the page");
        page[at..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect()
    }

    /// Log a machine key in (challenge signed with the key): a worker session.
    async fn login(&self, key: &SecretKey) -> String {
        let (s, ch) = self
            .api(
                "",
                "/worker/challenge",
                json!({ "public_key": key.public_key().to_hex() }),
            )
            .await;
        assert_eq!(s, 200);
        let body = LoginChallengeV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: self.network.clone(),
            action: "worker-login".into(),
            nonce: hex::decode(ch["nonce"].as_str().unwrap()).unwrap(),
            public_key: key.public_key(),
            issued_at: UnixMillis::now(),
        };
        let signed = Signed::sign(key, &self.network, Domain::Login, body).unwrap();
        let (s, sess) = self
            .api(
                "",
                "/worker/session",
                serde_json::to_value(&signed).unwrap(),
            )
            .await;
        assert_eq!(s, 200, "{sess}");
        sess["token"].as_str().unwrap().to_owned()
    }

    /// Register a machine key and log in with it.
    async fn machine(&self) -> (SecretKey, WorkerId, String) {
        let key = SecretKey::generate().unwrap();
        let (s, reg) = self
            .api(
                "",
                "/worker/register",
                json!({ "public_key": key.public_key().to_hex(), "display_name": "lab-pc" }),
            )
            .await;
        assert_eq!(s, 200, "{reg}");
        assert_eq!(reg["authorized"], false);
        let id: WorkerId = reg["worker_id"].as_str().unwrap().parse().unwrap();
        let session = self.login(&key).await;
        (key, id, session)
    }

    async fn claim(&self, session: &str) -> u16 {
        self.api(
            session,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await
        .0
    }
}

fn cookie_value(r: &reqwest::Response, name: &str) -> Option<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            let (k, rest) = c.split_once('=')?;
            let v = rest.split(';').next()?.to_owned();
            (k == name && !v.is_empty()).then_some(v)
        })
}

#[tokio::test]
async fn anyone_signs_up_and_their_machines_attach_themselves() {
    let env = start(true).await;
    let (s, page) = env.page("/portal/contribute", None).await;
    assert_eq!(s, 200);
    assert!(page.contains("Send me a code"), "{page}");

    // A wrong code, five times, kills it: even the right one no longer works.
    env.form(
        "/portal/contribute/email",
        &[("email", "Eve@Example.net")],
        None,
    )
    .await;
    let right = env.newest_code().await;
    for _ in 0..5 {
        let r = env
            .form(
                "/portal/contribute/verify",
                &[("email", "eve@example.net"), ("code", "00000000")],
                None,
            )
            .await;
        assert_eq!(r.status(), 200);
    }
    let r = env
        .form(
            "/portal/contribute/verify",
            &[("email", "eve@example.net"), ("code", &right)],
            None,
        )
        .await;
    assert_eq!(r.status(), 200, "the code died after five wrong attempts");
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("wrong, expired or already used")
    );

    // A fresh code signs up, with no operator involved.
    let session = env.sign_up("eve@example.net").await;
    let (_, page) = env.page("/portal/contribute", Some(&session)).await;
    assert!(
        page.contains("eve@example.net") && page.contains("No machine yet"),
        "{page}"
    );
    let code = env.new_enrollment_code(&session).await;

    // A machine that is not attached cannot work; attached with the code, it can at once.
    let (key, worker, machine) = env.machine().await;
    assert_eq!(env.claim(&machine).await, 403);
    let (s, _) = env
        .api("", "/worker/enroll", json!({ "enrollment_token": code }))
        .await;
    assert_eq!(s, 401, "enrolling needs the machine's own session");
    let (s, e) = env
        .api(
            &machine,
            "/worker/enroll",
            json!({ "enrollment_token": "gge_not-a-real-code-at-all" }),
        )
        .await;
    assert_eq!(s, 403, "{e}");
    let (s, e) = env
        .api(
            &machine,
            "/worker/enroll",
            json!({ "enrollment_token": code }),
        )
        .await;
    assert_eq!(s, 200, "{e}");
    assert_eq!(e["authorized"], true);
    assert_eq!(
        env.claim(&machine).await,
        204,
        "authorized at once: nothing to do, but allowed"
    );
    let (_, page) = env.page("/portal/contribute", Some(&session)).await;
    assert!(
        page.contains(&worker.to_string()) && page.contains("working"),
        "{page}"
    );

    // Another account cannot take this machine.
    let other = env.sign_up("mallory@example.net").await;
    let other_code = env.new_enrollment_code(&other).await;
    let (s, _) = env
        .api(
            &machine,
            "/worker/enroll",
            json!({ "enrollment_token": other_code }),
        )
        .await;
    assert_eq!(s, 409);

    // A new code replaces the old one: the old one attaches nothing more.
    let _newer = env.new_enrollment_code(&session).await;
    let (_, _, second) = env.machine().await;
    let (s, _) = env
        .api(
            &second,
            "/worker/enroll",
            json!({ "enrollment_token": code }),
        )
        .await;
    assert_eq!(s, 403);

    // The operator sees the account and suspends it: every machine stops at once.
    let list: Value = env
        .http
        .get(format!("{}/v1/admin/contributors", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let eve = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["email"] == "eve@example.net")
        .unwrap()
        .clone();
    assert_eq!(eve["machines"], 1);
    let id = eve["contributor_id"].as_str().unwrap();
    let (s, out) = env
        .api(
            &env.admin,
            &format!("/admin/contributors/{id}/suspend"),
            json!({ "suspended": true, "reason": "abuse" }),
        )
        .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["machines"], 1);
    assert_eq!(env.claim(&machine).await, 401, "its session was revoked");
    let (_, page) = env.page("/portal/contribute", Some(&session)).await;
    assert!(
        !page.contains("Your contribution"),
        "the contributor session was revoked: {page}"
    );
    // Reinstated: the machine works again once logged in.
    let (s, _) = env
        .api(
            &env.admin,
            &format!("/admin/contributors/{id}/suspend"),
            json!({ "suspended": false }),
        )
        .await;
    assert_eq!(s, 200);
    let machine = env.login(&key).await;
    assert_eq!(env.claim(&machine).await, 204);
    env.stop().await;
}

#[tokio::test]
async fn sign_up_is_closed_unless_the_operator_opens_it() {
    let env = start(false).await;
    let (s, _) = env.page("/portal/contribute", None).await;
    assert_eq!(s, 404);
    let mut cfg = ServerConfig {
        contributor_signup: true,
        ..ServerConfig::default()
    };
    assert!(cfg.validate().unwrap_err().contains("email_login"));
    cfg.dev_mode = true;
    cfg.email_login.provider = "simulated".into();
    assert!(cfg.validate().is_ok());
    env.stop().await;
}

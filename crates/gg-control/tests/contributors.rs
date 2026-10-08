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

impl Env {
    async fn json(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Value,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let mut req = if method == "GET" {
            self.http.get(url)
        } else {
            self.http.post(url).json(&body)
        };
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        let r = req.send().await.unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }
}

/// The contributor's application signs up, gets an enrollment code and attaches a machine without a
/// page or a cookie: the same account, the same rules as the forms.
#[tokio::test]
async fn the_application_signs_up_and_attaches_a_machine_with_json_alone() {
    let env = start(true).await;
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/code",
            None,
            json!({ "email": "not an address" }),
        )
        .await;
    assert_eq!(s, 422);
    let (s, sent) = env
        .json(
            "POST",
            "/contributor/v1/code",
            None,
            json!({ "email": "Ada@Example.org" }),
        )
        .await;
    assert_eq!(s, 200, "{sent}");
    assert_eq!(sent["email"], "ada@example.org");
    let code = env.newest_code().await;
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/session",
            None,
            json!({ "email": "ada@example.org", "code": "12345678" }),
        )
        .await;
    assert_eq!(s, 403, "a wrong code");
    let (s, session) = env
        .json(
            "POST",
            "/contributor/v1/session",
            None,
            json!({ "email": "ada@example.org", "code": code }),
        )
        .await;
    assert_eq!(s, 200, "{session}");
    let token = session["session"].as_str().unwrap().to_owned();
    assert!(token.starts_with("ggp_"));
    // The code is used: it does not open a second session.
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/session",
            None,
            json!({ "email": "ada@example.org", "code": code }),
        )
        .await;
    assert_eq!(s, 403);

    let (s, _) = env
        .json("GET", "/contributor/v1/me", None, Value::Null)
        .await;
    assert_eq!(s, 401, "no session");
    let (s, _) = env
        .json("GET", "/contributor/v1/me", Some("ggp_wrong"), Value::Null)
        .await;
    assert_eq!(s, 401);
    let (s, me) = env
        .json("GET", "/contributor/v1/me", Some(&token), Value::Null)
        .await;
    assert_eq!(s, 200, "{me}");
    assert_eq!(me["email"], "ada@example.org");
    assert_eq!(me["machines"].as_array().unwrap().len(), 0);

    let (s, e) = env
        .json(
            "POST",
            "/contributor/v1/enrollment",
            Some(&token),
            Value::Null,
        )
        .await;
    assert_eq!(s, 200, "{e}");
    let enroll = e["enrollment_token"].as_str().unwrap().to_owned();
    assert!(enroll.starts_with("gge_"));
    assert!(e["install_command"].as_str().unwrap().contains(&enroll));

    // A machine attaches itself with it and appears on the account.
    let (_key, id, machine) = env.machine().await;
    let (s, r) = env
        .api(
            &machine,
            "/worker/enroll",
            json!({ "enrollment_token": enroll }),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    let (_, me) = env
        .json("GET", "/contributor/v1/me", Some(&token), Value::Null)
        .await;
    assert_eq!(me["machines"][0]["worker_id"], id.to_string());
    assert_eq!(me["machines"][0]["working"], true);

    // The page's cookie and the application's session are the same account.
    let cookie = env.sign_up("ada@example.org").await;
    let (_, page) = env.page("/portal/contribute", Some(&cookie)).await;
    assert!(page.contains(&id.to_string()), "{page}");

    let (s, _) = env
        .json("POST", "/contributor/v1/logout", Some(&token), Value::Null)
        .await;
    assert_eq!(s, 204);
    let (s, _) = env
        .json("GET", "/contributor/v1/me", Some(&token), Value::Null)
        .await;
    assert_eq!(s, 401, "signed out");
    env.stop().await;
}

#[tokio::test]
async fn the_application_finds_sign_up_closed_when_the_operator_did_not_open_it() {
    let env = start(false).await;
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/code",
            None,
            json!({ "email": "ada@example.org" }),
        )
        .await;
    assert_eq!(s, 404);
    env.stop().await;
}

/// One account per person: a contributor signed in on the site asks for researcher access, an
/// operator reviews it (laboratory and purpose in the list), and only then can the account create
/// an API token, which works on the researcher API.
#[tokio::test]
async fn the_account_asks_for_researcher_access_and_gets_a_token_once_admitted() {
    let env = start(true).await;
    env.json(
        "POST",
        "/contributor/v1/code",
        None,
        json!({ "email": "marie@lab.example" }),
    )
    .await;
    let code = env.newest_code().await;
    let (_, s) = env
        .json(
            "POST",
            "/contributor/v1/session",
            None,
            json!({ "email": "marie@lab.example", "code": code }),
        )
        .await;
    let session = s["session"].as_str().unwrap().to_owned();
    let (s, r) = env
        .json(
            "GET",
            "/contributor/v1/research",
            Some(&session),
            Value::Null,
        )
        .await;
    assert_eq!((s, r["status"].as_str()), (200, Some("none")), "{r}");
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/research/token",
            Some(&session),
            Value::Null,
        )
        .await;
    assert_eq!(s, 403, "no token without researcher access");

    // Incomplete request: refused, nothing created.
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/research/request",
            Some(&session),
            json!({ "name": "Marie", "lab": "LAAS", "purpose": "too short" }),
        )
        .await;
    assert_eq!(s, 422);
    let request = json!({
        "name": "Marie Curie",
        "lab": "LAAS-CNRS",
        "purpose": "Train a small transformer on public protein sequences.",
    });
    let (s, r) = env
        .json(
            "POST",
            "/contributor/v1/research/request",
            Some(&session),
            request.clone(),
        )
        .await;
    assert_eq!((s, r["status"].as_str()), (200, Some("pending")), "{r}");
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/research/request",
            Some(&session),
            request,
        )
        .await;
    assert_eq!(s, 409, "a second request is refused");
    let (_, r) = env
        .json(
            "GET",
            "/contributor/v1/research",
            Some(&session),
            Value::Null,
        )
        .await;
    assert_eq!(r["status"], "pending");
    let (s, _) = env
        .json(
            "POST",
            "/contributor/v1/research/token",
            Some(&session),
            Value::Null,
        )
        .await;
    assert_eq!(s, 403, "no token while pending");

    // The operator sees the request and admits the researcher.
    let list = env
        .http
        .get(format!("{}/v1/admin/researchers", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let me = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["email"] == "marie@lab.example")
        .expect("the request in the admin list")
        .clone();
    assert_eq!(me["lab"], "LAAS-CNRS");
    assert_eq!(me["display_name"], "Marie Curie");
    assert!(me["purpose"].as_str().unwrap().contains("protein"));
    let id = me["researcher_id"].as_str().unwrap();
    let (s, _) = env
        .api(
            &env.admin,
            &format!("/admin/researchers/{id}/admission"),
            json!({ "state": "approved", "note": "pilot" }),
        )
        .await;
    assert!(s == 200 || s == 204, "{s}");

    let (s, t) = env
        .json(
            "POST",
            "/contributor/v1/research/token",
            Some(&session),
            Value::Null,
        )
        .await;
    assert_eq!(s, 200, "{t}");
    let token = t["token"].as_str().unwrap();
    assert!(token.starts_with("ggt_"));
    let me = env
        .http
        .get(format!("{}/v1/researcher/me", env.base))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        me.status().as_u16(),
        200,
        "the token works on the researcher API"
    );
    let (_, r) = env
        .json(
            "GET",
            "/contributor/v1/research",
            Some(&session),
            Value::Null,
        )
        .await;
    assert_eq!(r["status"], "approved");
    assert_eq!(r["projects"].as_array().unwrap().len(), 0);
    env.stop().await;
}

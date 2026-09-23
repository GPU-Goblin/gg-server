//! Portal / ORCID tests with the in-process simulator, plus the real-provider code path against a
//! local mock token endpoint (the real ORCID sandbox stays untested without credentials).

use axum::routing::post;
use axum::{Form, Json, Router};
use gg_control::config::ServerConfig;
use gg_control::orcid::{Endpoints, OrcidProvider};
use gg_control::server::serve;
use gg_control::state::AppState;
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;
use serde_json::{Value, json};
use zeroize::Zeroizing;

struct Env {
    tdb: Option<TestDb>,
    running: Option<gg_control::server::Running>,
    http: reqwest::Client,
    base: String,
    admin: String,
    _tmp: tempfile::TempDir,
}

async fn start() -> Env {
    start_with(|cfg| cfg.orcid.provider = "simulated".into()).await
}

async fn start_with(tune: impl FnOnce(&mut ServerConfig)) -> Env {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-portal".into(),
        dev_mode: true,
        ..ServerConfig::default()
    };
    tune(&mut cfg);
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
}

fn cookie_value(r: &reqwest::Response, name: &str) -> Option<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            let (k, rest) = c.split_once('=')?;
            if k == name {
                let v = rest.split(';').next()?.to_owned();
                if v.is_empty() { None } else { Some(v) }
            } else {
                None
            }
        })
}

/// Full simulated sign-in; returns (session cookie, login cookie used, state).
async fn sign_in(env: &Env, orcid: &str, name: &str) -> (String, String, String) {
    let r = env
        .http
        .get(format!("{}/portal/login", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "login must redirect");
    let location = r.headers()["location"].to_str().unwrap().to_owned();
    assert!(
        location.starts_with("/portal/dev/orcid/authorize?state="),
        "{location}"
    );
    let state = location.split("state=").nth(1).unwrap().to_owned();
    let login_cookie = cookie_value(&r, "gg_login").expect("login cookie");
    let r = env
        .http
        .get(format!("{}{location}", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("simul"));
    let r = env
        .http
        .post(format!("{}/portal/dev/orcid/authorize", env.base))
        .header("Sec-Fetch-Site", "same-origin")
        .form(&[
            ("state", state.as_str()),
            ("orcid", orcid),
            ("name", name),
            ("decision", "allow"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let cb = r.headers()["location"].to_str().unwrap().to_owned();
    assert!(cb.contains("/portal/callback?code="), "{cb}");
    let path = cb.split_once("/portal/callback").unwrap().1;
    let r = env
        .http
        .get(format!("{}/portal/callback{path}", env.base))
        .header("cookie", format!("gg_login={login_cookie}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let session = cookie_value(&r, "gg_session").expect("session cookie");
    (session, login_cookie, path.to_owned())
}

#[tokio::test]
async fn orcid_sign_in_admission_and_token_issuance() {
    let env = start().await;
    let (session, login_cookie, cb_path) =
        sign_in(&env, "0000-0002-1825-0097", "Josiah Carberry").await;

    // The callback cannot be replayed (state single-use) nor used without the login cookie.
    let r = env
        .http
        .get(format!("{}/portal/callback{cb_path}", env.base))
        .header("cookie", format!("gg_login={login_cookie}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = env
        .http
        .get(format!("{}/portal/callback{cb_path}", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // Pending admission: portal shows it, token issuance refused.
    let page = env
        .http
        .get(format!("{}/portal", env.base))
        .header("cookie", format!("gg_session={session}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("0000-0002-1825-0097") && page.contains("pending review"),
        "{page}"
    );
    let r = env
        .http
        .post(format!("{}/portal/token", env.base))
        .header("cookie", format!("gg_session={session}"))
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    // Admin sees the researcher and approves.
    let list: Value = env
        .http
        .get(format!("{}/v1/admin/researchers", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let me = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["orcid"] == "0000-0002-1825-0097")
        .expect("listed");
    assert_eq!(me["admission"], "pending");
    let id = me["researcher_id"].as_str().unwrap();
    let r = env
        .http
        .post(format!("{}/v1/admin/researchers/{id}/admission", env.base))
        .bearer_auth(&env.admin)
        .json(&json!({ "state": "approved", "note": "pilot" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // Cross-site POST is rejected by the CSRF layer; same-origin succeeds and shows the token once.
    let r = env
        .http
        .post(format!("{}/portal/token", env.base))
        .header("cookie", format!("gg_session={session}"))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = env
        .http
        .post(format!("{}/portal/token", env.base))
        .header("cookie", format!("gg_session={session}"))
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    let token = html
        .split("<code>")
        .nth(1)
        .unwrap()
        .split("</code>")
        .next()
        .unwrap()
        .to_owned();
    assert!(token.starts_with("ggt_"));
    let me: Value = env
        .http
        .get(format!("{}/v1/researcher/me", env.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["admission"], "approved");

    // Second sign-in with the same ORCID reuses the researcher; a bad checksum is refused.
    let (session2, _, _) = sign_in(&env, "0000-0002-1825-0097", "J. Carberry").await;
    let list: Value = env
        .http
        .get(format!("{}/v1/admin/researchers", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        list.as_array()
            .unwrap()
            .iter()
            .filter(|r| r["orcid"] == "0000-0002-1825-0097")
            .count(),
        1
    );
    let r = env
        .http
        .post(format!("{}/portal/dev/orcid/authorize", env.base))
        .header("Sec-Fetch-Site", "same-origin")
        .form(&[
            ("state", "x"),
            ("orcid", "0000-0002-1825-0098"),
            ("name", "Bad"),
            ("decision", "allow"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);

    // Logout revokes the session; admin revocation kills sessions and tokens.
    let r = env
        .http
        .post(format!("{}/portal/logout", env.base))
        .header("cookie", format!("gg_session={session2}"))
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let r = env
        .http
        .post(format!("{}/portal/token", env.base))
        .header("cookie", format!("gg_session={session2}"))
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303, "revoked session redirects to the portal");
    let r = env
        .http
        .post(format!("{}/v1/admin/researchers/{id}/revoke", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = env
        .http
        .get(format!("{}/v1/researcher/me", env.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    env.stop().await;
}

#[tokio::test]
async fn simulated_provider_is_refused_outside_dev_mode() {
    assert!(
        ServerConfig::default().validate().is_ok(),
        "default (disabled provider) must be valid"
    );
    let mut cfg = ServerConfig::default();
    cfg.orcid.provider = "simulated".into();
    cfg.dev_mode = false;
    assert!(cfg.validate().is_err());
    cfg.dev_mode = true;
    cfg.public = true;
    assert!(cfg.validate().is_err());
    let mut cfg = ServerConfig {
        dev_mode: true,
        ..ServerConfig::default()
    };
    cfg.orcid.provider = "sandbox".into();
    assert!(cfg.validate().is_err(), "client_id required");
    cfg.orcid.client_id = "APP-X".into();
    cfg.orcid.redirect_uri = "http://127.0.0.1:8080/portal/callback".into();
    assert!(cfg.validate().is_ok(), "http redirect allowed in dev_mode");
    cfg.dev_mode = false;
    assert!(cfg.validate().is_err(), "https required outside dev_mode");
}

/// The real-provider exchange is exercised against a local mock of the ORCID token endpoint.
#[tokio::test]
async fn real_provider_exchange_against_mock_token_endpoint() {
    #[derive(serde::Deserialize)]
    struct TokenForm {
        client_id: String,
        client_secret: String,
        grant_type: String,
        code: String,
        redirect_uri: String,
    }
    async fn token(Form(f): Form<TokenForm>) -> Json<Value> {
        assert_eq!(f.client_id, "APP-1");
        assert_eq!(f.client_secret, "SECRET-VALUE");
        assert_eq!(f.grant_type, "authorization_code");
        assert_eq!(f.redirect_uri, "http://127.0.0.1:9/portal/callback");
        if f.code == "good" {
            Json(
                json!({ "access_token": "at-1", "token_type": "bearer", "refresh_token": "rt", "expires_in": 631138518, "scope": "/authenticate", "name": "Sofia Garcia", "orcid": "0000-0001-5109-3700" }),
            )
        } else {
            Json(json!({ "error": "invalid_grant" }))
        }
    }
    let app = Router::new().route("/oauth/token", post(token));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let endpoints = Endpoints {
        authorize_url: format!("http://{addr}/oauth/authorize"),
        token_url: format!("http://{addr}/oauth/token"),
    };
    let p = OrcidProvider::real(
        endpoints,
        "APP-1".into(),
        Zeroizing::new("SECRET-VALUE".into()),
        "http://127.0.0.1:9/portal/callback".into(),
    )
    .unwrap();
    let id = p.exchange("good").await.unwrap();
    assert_eq!(id.orcid, "0000-0001-5109-3700");
    assert_eq!(id.name, "Sofia Garcia");
    assert!(p.exchange("bad").await.is_err());
}

// ---------------------------------------------------------------------------------------------
// Sign-in by e-mail link.

impl Env {
    async fn post_form(
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
            req = req.header("cookie", c);
        }
        req.send().await.unwrap()
    }

    async fn mailbox(&self) -> String {
        let r = self
            .http
            .get(format!("{}/portal/dev/mailbox", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.text().await.unwrap()
    }
}

/// The first sign-in token in a text: the newest one on the mailbox page, which lists the newest
/// message first, or the only one in a message.
fn newest_token(text: &str) -> Option<String> {
    let tail = text.split("/portal/email/confirm?token=").nth(1)?;
    Some(
        tail.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect(),
    )
}

/// Follow a link as a person would: open it (a page with a button, nothing consumed), then click.
async fn confirm(env: &Env, token: &str) -> reqwest::Response {
    let r = env
        .http
        .get(format!("{}/portal/email/confirm?token={token}", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Sign in to GPU Goblin"));
    env.post_form("/portal/email/confirm", &[("token", token)], None)
        .await
}

#[tokio::test]
async fn a_researcher_signs_in_with_a_university_address_and_waits_for_admission() {
    let env = start_with(|c| {
        c.email_login.provider = "simulated".into();
        c.email_login.allowed_domains = vec!["univ-paris.fr".into()];
    })
    .await;
    // The portal offers the e-mail form.
    let home = env
        .http
        .get(format!("{}/portal", env.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(home.contains("/portal/email"), "{home}");

    // Another domain is refused, and nothing is sent.
    let r = env
        .post_form("/portal/email", &[("email", "ada@gmail.com")], None)
        .await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("not accepted"));
    assert!(newest_token(&env.mailbox().await).is_none());

    // A subdomain of an accepted domain receives a link.
    let r = env
        .post_form(
            "/portal/email",
            &[("email", " Ada@Lab.Univ-Paris.fr ")],
            None,
        )
        .await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("ada@lab.univ-paris.fr"));
    let mailbox = env.mailbox().await;
    assert!(mailbox.contains("ada@lab.univ-paris.fr"), "{mailbox}");
    let token = newest_token(&mailbox).expect("a link in the message");

    // Opening the link twice consumes nothing; the click signs in once.
    let _ = env
        .http
        .get(format!("{}/portal/email/confirm?token={token}", env.base))
        .send()
        .await
        .unwrap();
    let r = confirm(&env, &token).await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let session = cookie_value(&r, "gg_session").expect("session cookie");
    assert!(session.starts_with("ggp_"), "{session}");
    let page = env
        .http
        .get(format!("{}/portal", env.base))
        .header("cookie", format!("gg_session={session}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("ada@lab.univ-paris.fr") && page.contains("pending review"),
        "{page}"
    );
    assert!(
        !page.contains("Create an API token"),
        "no token before admission: {page}"
    );

    // The link works once.
    let r = confirm(&env, &token).await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("already used"));

    // The operator sees the address and admits the researcher; signing in again reuses the account.
    let list: Value = env
        .http
        .get(format!("{}/v1/admin/researchers", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows: Vec<&Value> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["email"] == "ada@lab.univ-paris.fr")
        .collect();
    assert_eq!(rows.len(), 1, "{list}");
    assert_eq!(rows[0]["admission"], "pending");
    env.post_form("/portal/email", &[("email", "ada@lab.univ-paris.fr")], None)
        .await;
    let token = newest_token(&env.mailbox().await).unwrap();
    assert_eq!(confirm(&env, &token).await.status(), 303);
    let list: Value = env
        .http
        .get(format!("{}/v1/admin/researchers", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        list.as_array().unwrap().len(),
        1,
        "one account per address: {list}"
    );
    env.stop().await;
}

#[tokio::test]
async fn sign_in_links_are_limited_per_address_and_expire() {
    let env = start_with(|c| {
        c.email_login.provider = "simulated".into();
        c.email_login.max_links_per_hour = 2;
        c.email_login.link_ttl_secs = 60;
    })
    .await;
    for _ in 0..4 {
        let r = env
            .post_form("/portal/email", &[("email", "bob@univ.fr")], None)
            .await;
        assert_eq!(r.status(), 200, "the same answer, sent or not");
    }
    let mailbox = env.mailbox().await;
    assert_eq!(
        mailbox.matches("To <code>bob@univ.fr</code>").count(),
        2,
        "{mailbox}"
    );
    // An expired link is refused.
    let token = newest_token(&mailbox).unwrap();
    let mut c = env.tdb.as_ref().unwrap().db.pool().acquire().await.unwrap();
    sqlx::query("UPDATE email_login_links SET expires_at = now() - interval '1 second'")
        .execute(&mut *c)
        .await
        .unwrap();
    drop(c);
    let r = confirm(&env, &token).await;
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("expired"));
    env.stop().await;
}

#[tokio::test]
async fn simulated_mail_is_refused_outside_dev_mode_and_smtp_needs_a_relay() {
    let mut cfg = ServerConfig::default();
    cfg.email_login.provider = "simulated".into();
    assert!(cfg.validate().unwrap_err().contains("dev_mode"));
    cfg.dev_mode = true;
    cfg.public = true;
    assert!(cfg.validate().is_err());
    let mut cfg = ServerConfig::default();
    cfg.email_login.provider = "smtp".into();
    assert!(cfg.validate().unwrap_err().contains("smtp_host"));
    cfg.email_login.smtp_host = "smtp.example.org".into();
    cfg.email_login.from = "GPU Goblin <noreply@example.org>".into();
    cfg.email_login.smtp_tls = "none".into();
    assert!(cfg.validate().unwrap_err().contains("this machine"));
    cfg.email_login.smtp_tls = "starttls".into();
    assert!(cfg.validate().is_ok());
    cfg.public = true;
    assert!(
        cfg.validate().unwrap_err().contains("public_url"),
        "public needs an https public_url"
    );
    cfg.public_url = "https://grid.example.org".into();
    assert!(cfg.validate().is_ok());
}

/// The real SMTP path, against a minimal relay on this machine: the message leaves through lettre,
/// the relay receives it, and the link it carries signs in.
#[tokio::test]
async fn a_sign_in_link_travels_through_a_real_smtp_relay() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let relay = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = relay.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let (sock, _) = relay.accept().await.unwrap();
        let (r, mut w) = sock.into_split();
        let mut r = BufReader::new(r);
        w.write_all(b"220 relay ready\r\n").await.unwrap();
        let (mut data, mut in_data, mut tx) = (String::new(), false, Some(tx));
        let mut line = String::new();
        while r.read_line(&mut line).await.unwrap() > 0 {
            if in_data {
                if line == ".\r\n" {
                    in_data = false;
                    w.write_all(b"250 queued\r\n").await.unwrap();
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(data.clone());
                    }
                } else {
                    data.push_str(&line);
                }
            } else {
                let cmd = line.to_ascii_uppercase();
                let reply: &[u8] = if cmd.starts_with("EHLO") {
                    b"250-relay\r\n250 8BITMIME\r\n"
                } else if cmd.starts_with("DATA") {
                    in_data = true;
                    b"354 go ahead\r\n"
                } else if cmd.starts_with("QUIT") {
                    w.write_all(b"221 bye\r\n").await.unwrap();
                    break;
                } else {
                    b"250 ok\r\n"
                };
                w.write_all(reply).await.unwrap();
            }
            line.clear();
        }
    });
    let env = start_with(move |c| {
        c.email_login.provider = "smtp".into();
        c.email_login.smtp_host = "127.0.0.1".into();
        c.email_login.smtp_port = port;
        c.email_login.smtp_tls = "none".into();
        c.email_login.from = "GPU Goblin <noreply@grid.example.org>".into();
    })
    .await;
    let r = env
        .post_form("/portal/email", &[("email", "carol@univ.fr")], None)
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let message = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .expect("the relay received a message")
        .unwrap();
    assert!(
        message.contains("carol@univ.fr") && message.contains("sign-in link"),
        "{message}"
    );
    // Plain-text bodies may be quoted-printable: undo soft line breaks and =3D before reading.
    let body = message.replace("=\r\n", "").replace("=3D", "=");
    let token = newest_token(&body).expect("a link in the message");
    let r = confirm(&env, &token).await;
    assert_eq!(r.status(), 303);
    assert!(cookie_value(&r, "gg_session").is_some());
    env.stop().await;
}

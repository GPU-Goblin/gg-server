//! Minimal server-rendered portal: ORCID or e-mail sign-in, admission status, projects, API token
//! issuance.
//! Sessions are server-side (cookie holds a random id, stored hashed); state-changing routes are
//! protected by tower-http's CSRF layer (`Sec-Fetch-Site` / `Origin`).

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Router, extract::FromRequestParts, http::request::Parts};
use gg_core::*;
use gg_storage::queries::{identities, misc};
use serde::Deserialize;
use tower_http::csrf::CsrfLayer;

use crate::auth::{API_TOKEN_PREFIX, new_token, token_hash};
use crate::error::ApiError;
use crate::orcid::OrcidIdentity;
use crate::state::AppState;

const LOGIN_COOKIE: &str = "gg_login";
const SESSION_COOKIE: &str = "gg_session";
/// Cookie of a contributor's portal session (separate from researchers').
const CONTRIBUTOR_COOKIE: &str = "gg_contrib";
/// Prefix of portal session tokens.
const PORTAL_SESSION_PREFIX: &str = "ggp_";

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            if k == name && !v.is_empty() && v.len() <= 256 {
                Some(v.to_owned())
            } else {
                None
            }
        })
}

fn set_cookie(state: &AppState, name: &str, value: &str, max_age_secs: i64) -> HeaderValue {
    let secure = if state.config().dev_mode {
        ""
    } else {
        "; Secure"
    };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure}"
    ))
    .expect("cookie header")
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

fn page(state: &AppState, title: &str, body: &str) -> Html<String> {
    let banner = if state.orcid().is_simulated() || state.mailer().is_simulated() {
        "<p class=\"warn\">Development mode: sign-in (ORCID or e-mail) is simulated and does not verify who you are.</p>"
    } else {
        ""
    };
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{t} — GPU Goblin</title>\
<style>body{{font-family:system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem;color:#222}}\
.warn{{background:#fff3cd;border:1px solid #ffe08a;padding:.5rem 1rem}}code{{background:#f4f4f4;padding:0 .25rem}}\
table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:.25rem .5rem}}button{{padding:.4rem .8rem}}</style></head>\
<body><h1>GPU Goblin</h1>{banner}{body}<footer><p><small>Network <code>{net}</code></small></p></footer></body></html>",
        t = esc(title),
        net = esc(state.network().as_str())
    ))
}

/// Portal session extractor (researcher record, admission not required).
pub struct PortalSession {
    /// Researcher.
    pub researcher: gg_storage::models::ResearcherRow,
    /// Hash of the session token (for logout).
    pub session_hash: Sha256Digest,
}

impl FromRequestParts<AppState> for PortalSession {
    type Rejection = Redirect;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(token) = cookie(&parts.headers, SESSION_COOKIE) else {
            return Err(Redirect::to("/portal"));
        };
        let h = token_hash(&token);
        let mut c = state
            .db()
            .pool()
            .acquire()
            .await
            .map_err(|_| Redirect::to("/portal"))?;
        let Ok(Some(id)) = identities::lookup_portal_session(&mut c, &h).await else {
            return Err(Redirect::to("/portal"));
        };
        let Ok(Some(r)) = identities::get_researcher(&mut c, id).await else {
            return Err(Redirect::to("/portal"));
        };
        Ok(PortalSession {
            researcher: r,
            session_hash: h,
        })
    }
}

async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, ApiError> {
    let session = match cookie(&headers, SESSION_COOKIE) {
        Some(t) => {
            let mut c = state
                .db()
                .pool()
                .acquire()
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            match identities::lookup_portal_session(&mut c, &token_hash(&t)).await? {
                Some(id) => identities::get_researcher(&mut c, id).await?,
                None => None,
            }
        }
        None => None,
    };
    let body = match session {
        None if !state.orcid().is_enabled() && !state.mailer().is_enabled() => "<p>Signing in is not available on this server. An operator can give you an API token.</p>".to_owned(),
        None => {
            let mut b = "<p>Sign in to see your projects. You can submit computations once an operator has approved your access.</p>".to_owned();
            if state.orcid().is_enabled() {
                b.push_str("<p><a href=\"/portal/login\"><button type=\"button\">Sign in with ORCID</button></a></p>");
            }
            if state.mailer().is_enabled() {
                b.push_str(&email_form(&state));
            }
            b
        }
        Some(r) => {
            let mut c = state.db().pool().acquire().await.map_err(|e| ApiError::Internal(e.to_string()))?;
            let admission = r.admission().map_err(|e| ApiError::Internal(e.to_string()))?;
            let projects = identities::list_projects_for(&mut c, ResearcherId(r.id)).await?;
            let mut rows = String::new();
            for p in &projects {
                let role = identities::membership_role(&mut c, ProjectId(p.id), ResearcherId(r.id)).await?.map(|x| x.to_string()).unwrap_or_default();
                let b = gg_ledger::budget(&mut c, ProjectId(p.id)).await?;
                let project_state = match p.state.as_str() {
                    "active" => "Active",
                    "suspended" => "Suspended",
                    "closed" => "Closed",
                    other => other,
                };
                let role_label = match role.as_str() {
                    "owner" => "Owner",
                    "member" => "Member",
                    "viewer" => "Read only",
                    other => other,
                };
                rows.push_str(&format!(
                    "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{} / {} / {}</td></tr>",
                    p.id, esc(&p.name), esc(project_state), esc(role_label), b.available, b.reserved, b.spent
                ));
            }
            let projects_content = if projects.is_empty() {
                "<p>No project is linked to your account yet.</p>".to_owned()
            } else {
                format!("<table><tr><th>ID</th><th>Name</th><th>State</th><th>Role</th><th>Test credits (available / reserved / spent)</th></tr>{rows}</table>")
            };
            let admission_text = match admission {
                AdmissionState::Approved => "<span style=\"color:green\">approved</span>. You can create a token to use the command line.".to_owned(),
                AdmissionState::Pending => "<span style=\"color:#b36b00\">pending review</span>. An operator must approve your access before any submission.".to_owned(),
                AdmissionState::Suspended => "<span style=\"color:red\">suspended</span>. You cannot submit computations for now.".to_owned(),
                AdmissionState::Rejected => "<span style=\"color:red\">rejected</span>. You cannot submit computations.".to_owned(),
            };
            let token_form = if admission == AdmissionState::Approved {
                "<form method=\"post\" action=\"/portal/token\"><button type=\"submit\">Create an API token</button></form><p><small>The token will be shown only once.</small></p>"
            } else {
                ""
            };
            let identity = match (&r.orcid, identities::researcher_emails(&mut c).await?.get(&r.id)) {
                (Some(o), _) => format!("<p>Your ORCID iD: <a href=\"https://orcid.org/{o}\"><code>{o}</code></a></p>", o = esc(o)),
                (None, Some(e)) => format!("<p>Your e-mail: <code>{}</code> (verified)</p>", esc(e)),
                (None, None) => String::new(),
            };
            format!(
                "<h2>{name}</h2>{identity}\
<p>Your access: {admission_text}</p>{token_form}\
<h3>Your projects</h3>{projects_content}\
<form method=\"post\" action=\"/portal/logout\"><button type=\"submit\">Sign out</button></form>",
                name = esc(&r.display_name),
            )
        }
    };
    Ok(page(&state, "Researcher space", &body))
}

async fn login(State(state): State<AppState>) -> Result<Response, ApiError> {
    if !state.orcid().is_enabled() {
        return Err(ApiError::NotFound("ORCID sign-in is not configured".into()));
    }
    let st = new_token("")?;
    let nonce = new_token("")?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    identities::insert_oauth_state(
        &mut c,
        &token_hash(&st),
        &token_hash(&nonce),
        &state.config().orcid.provider,
        state.config().orcid.state_ttl_secs,
    )
    .await?;
    let mut resp = Redirect::to(&state.orcid().authorize_url(&st)).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(
            &state,
            LOGIN_COOKIE,
            &nonce,
            state.config().orcid.state_ttl_secs,
        ),
    );
    Ok(resp)
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    if let Some(e) = q.error {
        let (title, message) = if e == "access_denied" {
            (
                "Sign-in cancelled",
                "You did not allow the sign-in with ORCID.".to_owned(),
            )
        } else {
            (
                "Sign-in failed",
                format!(
                    "The sign-in with ORCID did not go through (code: {}).",
                    esc(&e)
                ),
            )
        };
        let body = format!("<p>{message}</p><p><a href=\"/portal\">Back to the start</a></p>");
        return Ok(page(&state, title, &body).into_response());
    }
    let (Some(code), Some(st)) = (q.code, q.state) else {
        return Err(ApiError::BadRequest("missing code or state".into()));
    };
    if st.len() > 256 {
        return Err(ApiError::BadRequest("invalid state".into()));
    }
    let Some(nonce) = cookie(&headers, LOGIN_COOKIE) else {
        return Err(ApiError::Forbidden(
            "login cookie missing: start again from /portal/login".into(),
        ));
    };
    let mut tx = state.db().begin().await?;
    if !identities::consume_oauth_state(&mut tx, &token_hash(&st), &token_hash(&nonce)).await? {
        return Err(ApiError::Forbidden(
            "unknown, expired or already used login state".into(),
        ));
    }
    tx.commit().await?;
    let identity: OrcidIdentity = state.orcid().exchange(&code).await?;
    let user_agent: String = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    let mut tx = state.db().begin().await?;
    let researcher = match identities::get_researcher_by_orcid(&mut tx, &identity.orcid).await? {
        Some(r) => {
            identities::touch_orcid_login(&mut tx, ResearcherId(r.id), &identity.name).await?;
            ResearcherId(r.id)
        }
        None => {
            let id = ResearcherId::new();
            let name = if identity.name.trim().is_empty() {
                identity.orcid.clone()
            } else {
                identity.name.clone()
            };
            identities::create_researcher_with_orcid(&mut tx, id, &name, &identity.orcid).await?;
            misc::audit(
                &mut tx,
                "portal",
                "researcher.orcid_signup",
                &id.to_string(),
                serde_json::json!({ "orcid": identity.orcid, "admission": "pending" }),
            )
            .await?;
            id
        }
    };
    let session = new_token(PORTAL_SESSION_PREFIX)?;
    identities::create_portal_session(
        &mut tx,
        &token_hash(&session),
        researcher,
        state.config().orcid.session_ttl_secs,
        &user_agent,
    )
    .await?;
    misc::audit(
        &mut tx,
        "portal",
        "researcher.login",
        &researcher.to_string(),
        serde_json::json!({ "provider": state.config().orcid.provider }),
    )
    .await?;
    tx.commit().await?;
    let mut resp = Redirect::to("/portal").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(
            &state,
            SESSION_COOKIE,
            &session,
            state.config().orcid.session_ttl_secs,
        ),
    );
    resp.headers_mut()
        .append(header::SET_COOKIE, set_cookie(&state, LOGIN_COOKIE, "", 0));
    Ok(resp)
}

async fn logout(
    State(state): State<AppState>,
    session: PortalSession,
) -> Result<Response, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    identities::revoke_portal_session(&mut c, &session.session_hash).await?;
    let mut resp = Redirect::to("/portal").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&state, SESSION_COOKIE, "", 0),
    );
    Ok(resp)
}

async fn issue_token(
    State(state): State<AppState>,
    session: PortalSession,
) -> Result<Response, ApiError> {
    let admission = session
        .researcher
        .admission()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if admission != AdmissionState::Approved {
        return Err(ApiError::Forbidden(format!("admission is {admission}")));
    }
    let token = new_token(API_TOKEN_PREFIX)?;
    let mut tx = state.db().begin().await?;
    identities::create_token(
        &mut tx,
        &token_hash(&token),
        "researcher",
        Some(ResearcherId(session.researcher.id)),
        "portal",
        None,
    )
    .await?;
    misc::audit(
        &mut tx,
        "portal",
        "researcher.token.create",
        &session.researcher.id.to_string(),
        serde_json::json!({}),
    )
    .await?;
    tx.commit().await?;
    let body = format!(
        "<h2>API token created</h2><p>This token is shown only once. Copy it and keep it somewhere safe:</p><pre><code>{t}</code></pre>\
<p>To use it with the command line: <code>gg auth login --server {base} --token &lt;your_token&gt;</code></p><p><a href=\"/portal\">Back</a></p>",
        t = esc(&token),
        base = esc(&state.config().public_url)
    );
    Ok(page(&state, "API token", &body).into_response())
}

#[derive(Deserialize)]
struct DevAuthorizeQuery {
    state: String,
}

async fn dev_authorize_form(
    State(state): State<AppState>,
    Query(q): Query<DevAuthorizeQuery>,
) -> Result<Response, ApiError> {
    if !state.orcid().is_simulated() {
        return Err(ApiError::NotFound("simulator disabled".into()));
    }
    let body = format!(
        "<h2>Simulated ORCID sign-in</h2><p>This page is for local testing. It does not verify who you are.</p><form method=\"post\" action=\"/portal/dev/orcid/authorize\">\
<input type=\"hidden\" name=\"state\" value=\"{st}\">\
<p><label>ORCID iD <input name=\"orcid\" value=\"0000-0002-1825-0097\" size=\"22\"></label></p>\
<p><label>Display name <input name=\"name\" value=\"Josiah Carberry\" size=\"30\"></label></p>\
<button type=\"submit\" name=\"decision\" value=\"allow\">Continue</button> <button type=\"submit\" name=\"decision\" value=\"deny\">Cancel</button></form>",
        st = esc(&q.state)
    );
    Ok(page(&state, "Simulated ORCID sign-in", &body).into_response())
}

#[derive(Deserialize)]
struct DevAuthorizeForm {
    state: String,
    orcid: String,
    name: String,
    decision: String,
}

async fn dev_authorize_submit(
    State(state): State<AppState>,
    Form(f): Form<DevAuthorizeForm>,
) -> Result<Response, ApiError> {
    if !state.orcid().is_simulated() {
        return Err(ApiError::NotFound("simulator disabled".into()));
    }
    let redirect = state.orcid().redirect_uri().to_owned();
    if f.decision != "allow" {
        return Ok(Redirect::to(&format!(
            "{redirect}?error=access_denied&state={}",
            crate::orcid::urlencode(&f.state)
        ))
        .into_response());
    }
    let code = state.orcid().simulated_issue_code(OrcidIdentity {
        orcid: f.orcid.trim().to_owned(),
        name: f.name.chars().take(128).collect(),
    })?;
    Ok(Redirect::to(&format!(
        "{redirect}?code={}&state={}",
        crate::orcid::urlencode(&code),
        crate::orcid::urlencode(&f.state)
    ))
    .into_response())
}

fn email_form(state: &AppState) -> String {
    let domains = &state.config().email_login.allowed_domains;
    let hint = if domains.is_empty() {
        String::new()
    } else {
        format!(
            " Accepted domains: {}.",
            domains
                .iter()
                .map(|d| format!("<code>{}</code>", esc(d)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "<form method=\"post\" action=\"/portal/email\"><p><label>Or with your institutional e-mail address \
<input type=\"email\" name=\"email\" required maxlength=\"254\" size=\"32\"></label> \
<button type=\"submit\">Send me a sign-in link</button></p><p><small>The link is valid for {} minutes and works once.{hint}</small></p></form>",
        state.config().email_login.link_ttl_secs / 60
    )
}

/// Normalize and check an address: trimmed, lowercase, one `@`, a dotted domain, no spaces or
/// control characters, at most 254 bytes.
pub fn normalize_email(raw: &str) -> Option<String> {
    let e = raw.trim().to_ascii_lowercase();
    if e.len() < 3 || e.len() > 254 || e.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (local, domain) = e.split_once('@')?;
    let ok = !local.is_empty()
        && local.len() <= 64
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then_some(e)
}

/// Whether the address's domain is accepted: any when the list is empty, otherwise an accepted
/// domain or one of its subdomains.
pub fn email_domain_allowed(email: &str, allowed: &[String]) -> bool {
    let Some((_, domain)) = email.split_once('@') else {
        return false;
    };
    allowed.is_empty()
        || allowed
            .iter()
            .any(|a| domain == a || domain.ends_with(&format!(".{a}")))
}

#[derive(Deserialize)]
struct EmailRequest {
    email: String,
}

async fn email_request(
    State(state): State<AppState>,
    Form(f): Form<EmailRequest>,
) -> Result<Response, ApiError> {
    if !state.mailer().is_enabled() {
        return Err(ApiError::NotFound(
            "e-mail sign-in is not configured".into(),
        ));
    }
    let cfg = &state.config().email_login;
    let Some(email) = normalize_email(&f.email) else {
        return Ok(page(
            &state,
            "Sign in",
            "<p>This is not a valid e-mail address.</p><p><a href=\"/portal\">Back</a></p>",
        )
        .into_response());
    };
    if !email_domain_allowed(&email, &cfg.allowed_domains) {
        return Ok(page(&state, "Sign in", "<p>Addresses of this domain are not accepted on this server. Ask its operator, or sign in with ORCID if it is offered.</p><p><a href=\"/portal\">Back</a></p>").into_response());
    }
    // Same answer whether or not a link was sent: the page says nothing about who has an account.
    let answer = format!(
        "<p>If this address can sign in, a link has been sent to <code>{}</code>. It is valid for {} minutes and works once.</p><p><a href=\"/portal\">Back</a></p>",
        esc(&email),
        cfg.link_ttl_secs / 60
    );
    let mut c = state.db().pool().acquire().await?;
    if identities::count_recent_email_links(&mut c, &email, 3600).await? >= cfg.max_links_per_hour {
        tracing::info!("e-mail sign-in link not sent: hourly limit reached for an address");
        return Ok(page(&state, "Sign in", &answer).into_response());
    }
    let token = new_token("")?;
    identities::insert_email_link(&mut c, &token_hash(&token), &email, cfg.link_ttl_secs).await?;
    drop(c);
    let link = format!(
        "{}/portal/email/confirm?token={token}",
        state.config().public_url
    );
    let body = format!(
        "Someone (hopefully you) asked to sign in to GPU Goblin with this address.\n\n\
Open this link to sign in; it is valid for {} minutes and works once:\n\n{link}\n\n\
If you did not ask for it, ignore this message: nothing happens without the link.\n",
        cfg.link_ttl_secs / 60
    );
    state
        .mailer()
        .send(&email, "Your GPU Goblin sign-in link", &body)
        .await?;
    Ok(page(&state, "Sign in", &answer).into_response())
}

#[derive(Deserialize)]
struct EmailToken {
    token: String,
}

/// Opening the link only shows a button: mail scanners that follow links must not consume it.
async fn email_confirm_form(
    State(state): State<AppState>,
    Query(q): Query<EmailToken>,
) -> Result<Response, ApiError> {
    if !state.mailer().is_enabled() {
        return Err(ApiError::NotFound(
            "e-mail sign-in is not configured".into(),
        ));
    }
    if q.token.is_empty() || q.token.len() > 128 {
        return Err(ApiError::BadRequest("invalid link".into()));
    }
    let body = format!(
        "<h2>Sign in</h2><form method=\"post\" action=\"/portal/email/confirm\">\
<input type=\"hidden\" name=\"token\" value=\"{}\"><button type=\"submit\">Sign in to GPU Goblin</button></form>",
        esc(&q.token)
    );
    Ok(page(&state, "Sign in", &body).into_response())
}

async fn email_confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<EmailToken>,
) -> Result<Response, ApiError> {
    if !state.mailer().is_enabled() {
        return Err(ApiError::NotFound(
            "e-mail sign-in is not configured".into(),
        ));
    }
    if f.token.is_empty() || f.token.len() > 128 {
        return Err(ApiError::BadRequest("invalid link".into()));
    }
    let user_agent: String = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    let mut tx = state.db().begin().await?;
    let Some(email) = identities::consume_email_link(&mut tx, &token_hash(&f.token)).await? else {
        return Ok(page(&state, "Sign in", "<p>This link is unknown, expired or already used. Ask for a new one.</p><p><a href=\"/portal\">Back</a></p>").into_response());
    };
    let researcher = match identities::get_researcher_by_email(&mut tx, &email).await? {
        Some(r) => {
            identities::touch_email_login(&mut tx, ResearcherId(r.id)).await?;
            ResearcherId(r.id)
        }
        None => {
            let id = ResearcherId::new();
            identities::create_researcher_with_email(&mut tx, id, &email).await?;
            misc::audit(
                &mut tx,
                "portal",
                "researcher.email_signup",
                &id.to_string(),
                serde_json::json!({ "email": email, "admission": "pending" }),
            )
            .await?;
            id
        }
    };
    let ttl = state.config().email_login.session_ttl_secs;
    let session = new_token(PORTAL_SESSION_PREFIX)?;
    identities::create_portal_session(&mut tx, &token_hash(&session), researcher, ttl, &user_agent)
        .await?;
    misc::audit(
        &mut tx,
        "portal",
        "researcher.login",
        &researcher.to_string(),
        serde_json::json!({ "provider": "email" }),
    )
    .await?;
    tx.commit().await?;
    let mut resp = Redirect::to("/portal").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&state, SESSION_COOKIE, &session, ttl),
    );
    Ok(resp)
}

// ---------------------------------------------------------------------------------------------
// Contributors (ADR-0030): sign-up with an e-mail code, no admission, machines attached by an
// enrollment code.

fn contributor_signup_enabled(state: &AppState) -> Result<(), ApiError> {
    if state.config().contributor_signup && state.mailer().is_enabled() {
        Ok(())
    } else {
        Err(ApiError::NotFound(
            "contributor sign-up is not open on this server".into(),
        ))
    }
}

async fn contributor_of(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<gg_storage::queries::contributors::ContributorRow>, ApiError> {
    let Some(token) = cookie(headers, CONTRIBUTOR_COOKIE) else {
        return Ok(None);
    };
    let mut c = state.db().pool().acquire().await?;
    let Some(id) =
        gg_storage::queries::contributors::lookup_session(&mut c, &token_hash(&token)).await?
    else {
        return Ok(None);
    };
    Ok(gg_storage::queries::contributors::get(&mut c, id).await?)
}

fn install_command(state: &AppState, token: &str) -> String {
    format!(
        "curl -fsSLO {base}/releases/latest/install.sh\nbash install.sh --enroll {token}          # add --gpu (Vulkan) or --cuda (NVIDIA)",
        base = state.config().public_url
    )
}

async fn contribute(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let Some(account) = contributor_of(&state, &headers).await? else {
        let body = "<h2>Lend a machine to research</h2>\
<p>Your computer runs the platform's own code, inside a sandbox, on other people's research tasks: never a researcher's code. \
The machine that computes a task sees its data; sharing with other contributors makes your address visible to them. \
Read what you agree to before installing.</p>\
<form method=\"post\" action=\"/portal/contribute/email\"><p><label>Your e-mail address \
<input type=\"email\" name=\"email\" required maxlength=\"254\" size=\"32\"></label> \
<button type=\"submit\">Send me a code</button></p></form>";
        return Ok(page(&state, "Contribute", body).into_response());
    };
    if account.suspended_at.is_some() {
        let body = format!(
            "<h2>Account suspended</h2><p>{}</p><p>Your machines no longer receive work. Contact the operators.</p>",
            esc(&account.suspended_reason)
        );
        return Ok(page(&state, "Contribute", &body).into_response());
    }
    let mut c = state.db().pool().acquire().await?;
    let machines = gg_storage::queries::contributors::machines(&mut c, account.id).await?;
    let mut rows = String::new();
    for m in &machines {
        rows.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&m.display_name),
            m.id,
            if m.authorized { "working" } else { "stopped" },
            m.last_seen_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| "never".into()),
            m.accepted
        ));
    }
    let table = if machines.is_empty() {
        "<p>No machine yet. Create an enrollment code, then run the install command on the machine.</p>".to_owned()
    } else {
        format!(
            "<table><tr><th>name</th><th>id</th><th>state</th><th>last seen</th><th>accepted tasks</th></tr>{rows}</table>"
        )
    };
    let body = format!(
        "<h2>Your contribution</h2><p>Signed in as <code>{}</code>.</p><h3>Machines</h3>{table}\
<h3>Add a machine</h3><form method=\"post\" action=\"/portal/contribute/token\"><button type=\"submit\">Create an enrollment code (replaces the previous one)</button></form>\
<form method=\"post\" action=\"/portal/contribute/logout\"><p><button type=\"submit\">Sign out</button></p></form>",
        esc(&account.email)
    );
    Ok(page(&state, "Contribute", &body).into_response())
}

#[derive(Deserialize)]
struct ContributeEmail {
    email: String,
}

async fn contribute_email(
    State(state): State<AppState>,
    Form(f): Form<ContributeEmail>,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let Some(email) = normalize_email(&f.email) else {
        return Ok(page(&state, "Contribute", "<p>This is not a valid e-mail address.</p><p><a href=\"/portal/contribute\">Back</a></p>").into_response());
    };
    crate::workflows::contributors::send_code(&state, &email).await?;
    // Same answer whether or not a code was sent (hourly limit): nothing about the account leaks.
    let body = format!(
        "<p>If this address can receive one, a code has been sent to <code>{e}</code>. It is valid for {m} minutes.</p>\
<form method=\"post\" action=\"/portal/contribute/verify\"><input type=\"hidden\" name=\"email\" value=\"{e}\">\
<p><label>Code <input name=\"code\" inputmode=\"numeric\" autocomplete=\"one-time-code\" maxlength=\"8\" size=\"10\" required></label> \
<button type=\"submit\">Verify</button></p></form>",
        e = esc(&email),
        m = crate::workflows::contributors::CODE_TTL_SECS / 60
    );
    Ok(page(&state, "Contribute", &body).into_response())
}

#[derive(Deserialize)]
struct ContributeVerify {
    email: String,
    code: String,
}

async fn contribute_verify(
    State(state): State<AppState>,
    Form(f): Form<ContributeVerify>,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let (Some(email), true) = (normalize_email(&f.email), f.code.trim().len() <= 16) else {
        return Err(ApiError::BadRequest("invalid form".into()));
    };
    let Some(account) =
        crate::workflows::contributors::verify_code(&state, &email, &f.code).await?
    else {
        return Ok(page(&state, "Contribute", "<p>This code is wrong, expired or already used. Ask for a new one.</p><p><a href=\"/portal/contribute\">Back</a></p>").into_response());
    };
    let ttl = state.config().email_login.session_ttl_secs;
    let session = new_token(PORTAL_SESSION_PREFIX)?;
    let mut c = state.db().pool().acquire().await?;
    gg_storage::queries::contributors::create_session(&mut c, &token_hash(&session), account, ttl)
        .await?;
    let mut resp = Redirect::to("/portal/contribute").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&state, CONTRIBUTOR_COOKIE, &session, ttl),
    );
    Ok(resp)
}

async fn contribute_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let Some(account) = contributor_of(&state, &headers).await? else {
        return Ok(Redirect::to("/portal/contribute").into_response());
    };
    if account.suspended_at.is_some() {
        return Err(ApiError::Forbidden("account suspended".into()));
    }
    let token = crate::workflows::contributors::new_enrollment_token(&state, account.id).await?;
    let body = format!(
        "<h2>Enrollment code</h2><p>Copy it now: it is shown once. It attaches machines to your account; creating a new one disables this one \
(machines already attached keep working).</p><pre><code>{t}</code></pre>\
<p>On the machine (Linux x86_64, see the installation guide for the requirements and for checking the release key):</p><pre><code>{cmd}</code></pre>\
<p><a href=\"/portal/contribute\">Back</a></p>",
        t = esc(&token),
        cmd = esc(&install_command(&state, &token))
    );
    Ok(page(&state, "Enrollment code", &body).into_response())
}

async fn contribute_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = cookie(&headers, CONTRIBUTOR_COOKIE) {
        let mut c = state.db().pool().acquire().await?;
        gg_storage::queries::contributors::revoke_session(&mut c, &token_hash(&token)).await?;
    }
    let mut resp = Redirect::to("/portal/contribute").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&state, CONTRIBUTOR_COOKIE, "", 0),
    );
    Ok(resp)
}

// ---------------------------------------------------------------------------------------------
// The same contributor account as JSON, for the contributor's application (ADR-0043): no page, no
// cookie; the session travels as a bearer token (so no cross-site request can use it, and the
// CSRF layer of the forms does not apply). Same rules as the forms: the same answer whether or not
// a code was sent, five checks per code, the account created on the first right code.

fn contributor_bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ")?.trim();
    (t.starts_with(PORTAL_SESSION_PREFIX) && t.len() <= 128).then(|| t.to_owned())
}

async fn contributor_by_bearer(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<gg_storage::queries::contributors::ContributorRow, ApiError> {
    let token = contributor_bearer(headers).ok_or(ApiError::Unauthorized)?;
    let mut c = state.db().pool().acquire().await?;
    let id = gg_storage::queries::contributors::lookup_session(&mut c, &token_hash(&token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    gg_storage::queries::contributors::get(&mut c, id)
        .await?
        .ok_or(ApiError::Unauthorized)
}

#[derive(Deserialize)]
struct CodeRequest {
    email: String,
}

async fn api_code(
    State(state): State<AppState>,
    axum::Json(r): axum::Json<CodeRequest>,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let email = normalize_email(&r.email)
        .ok_or_else(|| ApiError::Unprocessable("this is not a valid e-mail address".into()))?;
    crate::workflows::contributors::send_code(&state, &email).await?;
    Ok(axum::Json(serde_json::json!({
        "email": email,
        "valid_secs": crate::workflows::contributors::CODE_TTL_SECS,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct SessionRequest {
    email: String,
    code: String,
}

async fn api_session(
    State(state): State<AppState>,
    axum::Json(r): axum::Json<SessionRequest>,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let (Some(email), true) = (normalize_email(&r.email), r.code.trim().len() <= 16) else {
        return Err(ApiError::Unprocessable(
            "invalid e-mail address or code".into(),
        ));
    };
    let Some(account) =
        crate::workflows::contributors::verify_code(&state, &email, &r.code).await?
    else {
        return Err(ApiError::Forbidden(
            "this code is wrong, expired or already used".into(),
        ));
    };
    let ttl = state.config().email_login.session_ttl_secs;
    let session = new_token(PORTAL_SESSION_PREFIX)?;
    let mut c = state.db().pool().acquire().await?;
    gg_storage::queries::contributors::create_session(&mut c, &token_hash(&session), account, ttl)
        .await?;
    Ok(axum::Json(serde_json::json!({
        "session": session,
        "expires_in_secs": ttl,
        "email": email,
    }))
    .into_response())
}

async fn api_me(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let account = contributor_by_bearer(&state, &headers).await?;
    let mut c = state.db().pool().acquire().await?;
    let machines = gg_storage::queries::contributors::machines(&mut c, account.id).await?;
    let machines: Vec<serde_json::Value> = machines
        .iter()
        .map(|m| {
            serde_json::json!({
                "worker_id": m.id,
                "name": m.display_name,
                "working": m.authorized,
                "last_seen_at": m.last_seen_at.map(|t| t.to_string()),
                "accepted": m.accepted,
            })
        })
        .collect();
    Ok(axum::Json(serde_json::json!({
        "email": account.email,
        "suspended": account.suspended_at.is_some(),
        "suspended_reason": account.suspended_reason,
        "machines": machines,
    }))
    .into_response())
}

async fn api_enrollment(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let account = contributor_by_bearer(&state, &headers).await?;
    if account.suspended_at.is_some() {
        return Err(ApiError::Forbidden("account suspended".into()));
    }
    let token = crate::workflows::contributors::new_enrollment_token(&state, account.id).await?;
    Ok(axum::Json(serde_json::json!({
        "enrollment_token": token,
        "install_command": install_command(&state, &token),
    }))
    .into_response())
}

async fn api_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = contributor_bearer(&headers) {
        let mut c = state.db().pool().acquire().await?;
        gg_storage::queries::contributors::revoke_session(&mut c, &token_hash(&token)).await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// The same account's researcher side (one account per person, the owner's decision): the
// researcher record of the account's verified address, an access request reviewed by an
// operator, and API tokens once admitted. Same rules as the portal: admission is a human decision,
// the address's domain must be accepted, a token is shown once.

/// The researcher record of the signed-in account, by its verified address.
async fn account_researcher(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<
    (
        gg_storage::queries::contributors::ContributorRow,
        Option<gg_storage::models::ResearcherRow>,
    ),
    ApiError,
> {
    let account = contributor_by_bearer(state, headers).await?;
    let mut c = state.db().pool().acquire().await?;
    let researcher = identities::get_researcher_by_email(&mut c, &account.email).await?;
    Ok((account, researcher))
}

async fn api_research(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let (_, researcher) = account_researcher(&state, &headers).await?;
    let Some(r) = researcher else {
        return Ok(axum::Json(serde_json::json!({ "status": "none" })).into_response());
    };
    let mut c = state.db().pool().acquire().await?;
    let mut projects = Vec::new();
    for p in identities::list_projects_for(&mut c, ResearcherId(r.id)).await? {
        let role = identities::membership_role(&mut c, ProjectId(p.id), ResearcherId(r.id))
            .await?
            .map(|x| x.to_string())
            .unwrap_or_default();
        let b = gg_ledger::budget(&mut c, ProjectId(p.id)).await?;
        projects.push(serde_json::json!({
            "project_id": p.id,
            "name": p.name,
            "state": p.state,
            "role": role,
            "available": b.available.to_string(),
            "reserved": b.reserved.to_string(),
            "spent": b.spent.to_string(),
        }));
    }
    Ok(axum::Json(serde_json::json!({
        "status": r.admission_state,
        "name": r.display_name,
        "projects": projects,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct AccessRequest {
    name: String,
    lab: String,
    purpose: String,
}

/// A line of text a person typed: trimmed, within bounds, no control characters.
fn typed(s: &str, min: usize, max: usize) -> Option<String> {
    let t = s.trim();
    let n = t.chars().count();
    (n >= min && n <= max && !t.chars().any(|c| c.is_control() && c != '\n')).then(|| t.to_owned())
}

async fn api_research_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(q): axum::Json<AccessRequest>,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let (account, researcher) = account_researcher(&state, &headers).await?;
    if account.suspended_at.is_some() {
        return Err(ApiError::Forbidden("account suspended".into()));
    }
    if let Some(r) = researcher {
        return Err(ApiError::Conflict(format!(
            "access already requested: {}",
            r.admission_state
        )));
    }
    let (Some(name), Some(lab), Some(purpose)) = (
        typed(&q.name, 2, 100),
        typed(&q.lab, 2, 200),
        typed(&q.purpose, 20, 2000),
    ) else {
        return Err(ApiError::Unprocessable(
            "name (2-100 characters), laboratory (2-200) and purpose (20-2000) are required".into(),
        ));
    };
    if !email_domain_allowed(&account.email, &state.config().email_login.allowed_domains) {
        return Err(ApiError::Forbidden(
            "researcher access is not open to this address's domain".into(),
        ));
    }
    let id = ResearcherId(uuid::Uuid::now_v7());
    let mut tx = state.db().begin().await?;
    identities::create_access_request(&mut tx, id, &account.email, &name, &lab, &purpose).await?;
    misc::audit(
        &mut tx,
        "portal",
        "researcher.access_request",
        &id.to_string(),
        serde_json::json!({ "email": account.email, "lab": lab }),
    )
    .await?;
    tx.commit().await?;
    Ok(axum::Json(serde_json::json!({ "status": "pending", "name": name })).into_response())
}

async fn api_research_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    contributor_signup_enabled(&state)?;
    let (_, researcher) = account_researcher(&state, &headers).await?;
    let r = researcher.ok_or_else(|| ApiError::Forbidden("no researcher access".into()))?;
    let admission = r
        .admission()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if admission != AdmissionState::Approved {
        return Err(ApiError::Forbidden(format!("admission is {admission}")));
    }
    let token = new_token(API_TOKEN_PREFIX)?;
    let mut tx = state.db().begin().await?;
    identities::create_token(
        &mut tx,
        &token_hash(&token),
        "researcher",
        Some(ResearcherId(r.id)),
        "account",
        None,
    )
    .await?;
    misc::audit(
        &mut tx,
        "portal",
        "researcher.token.create",
        &r.id.to_string(),
        serde_json::json!({ "from": "account" }),
    )
    .await?;
    tx.commit().await?;
    Ok(axum::Json(serde_json::json!({
        "token": token,
        "server": state.config().public_url,
    }))
    .into_response())
}

/// The contributor account as JSON (mounted at `/contributor/v1`), for the application.
pub fn contributor_api(state: AppState) -> Router {
    Router::new()
        .route("/contributor/v1/code", post(api_code))
        .route("/contributor/v1/session", post(api_session))
        .route("/contributor/v1/me", get(api_me))
        .route("/contributor/v1/enrollment", post(api_enrollment))
        .route("/contributor/v1/logout", post(api_logout))
        .route("/contributor/v1/research", get(api_research))
        .route(
            "/contributor/v1/research/request",
            post(api_research_request),
        )
        .route("/contributor/v1/research/token", post(api_research_token))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(16 * 1024))
        .with_state(state)
}

/// Development only: the messages the simulated mailer kept.
async fn dev_mailbox(State(state): State<AppState>) -> Result<Response, ApiError> {
    if !state.mailer().is_simulated() {
        return Err(ApiError::NotFound("simulated mailer disabled".into()));
    }
    let mut rows = String::new();
    for m in state.mailer().simulated_outbox().iter().rev() {
        rows.push_str(&format!(
            "<h3>To <code>{}</code>: {}</h3><pre>{}</pre>",
            esc(&m.to),
            esc(&m.subject),
            esc(&m.body)
        ));
    }
    Ok(page(
        &state,
        "Mailbox (development)",
        &format!("<h2>Simulated mailbox</h2>{rows}"),
    )
    .into_response())
}

/// Portal router (mounted at `/portal`).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/portal", get(index))
        .route("/portal/login", get(login))
        .route("/portal/callback", get(callback))
        .route("/portal/logout", post(logout))
        .route("/portal/token", post(issue_token))
        .route("/portal/email", post(email_request))
        .route(
            "/portal/email/confirm",
            get(email_confirm_form).post(email_confirm),
        )
        .route("/portal/dev/mailbox", get(dev_mailbox))
        .route("/portal/contribute", get(contribute))
        .route("/portal/contribute/email", post(contribute_email))
        .route("/portal/contribute/verify", post(contribute_verify))
        .route("/portal/contribute/token", post(contribute_token))
        .route("/portal/contribute/logout", post(contribute_logout))
        .route(
            "/portal/dev/orcid/authorize",
            get(dev_authorize_form).post(dev_authorize_submit),
        )
        .layer(CsrfLayer::new())
        .layer(tower_http::limit::RequestBodyLimitLayer::new(16 * 1024))
        .with_state(state)
}

/// Response helper used by tests.
pub fn status_of(r: &Response) -> StatusCode {
    r.status()
}

#[cfg(test)]
mod tests {
    use super::{email_domain_allowed, normalize_email};

    #[test]
    fn addresses_are_normalized_and_checked() {
        assert_eq!(
            normalize_email("  Ada.Lovelace@Univ-Paris.FR "),
            Some("ada.lovelace@univ-paris.fr".into())
        );
        for bad in [
            "",
            "ada",
            "ada@",
            "@univ.fr",
            "ada@univ",
            "a b@univ.fr",
            "ada@univ.fr.",
            "ada@@univ.fr",
            "ada@univ_x.fr",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad}");
        }
        assert_eq!(
            normalize_email(&format!("{}@univ.fr", "a".repeat(65))),
            None
        );
    }

    #[test]
    fn a_domain_is_accepted_with_its_subdomains_only() {
        let allowed = vec!["univ-paris.fr".to_owned(), "cnrs.fr".to_owned()];
        assert!(email_domain_allowed("ada@univ-paris.fr", &allowed));
        assert!(email_domain_allowed("ada@lab.cnrs.fr", &allowed));
        assert!(!email_domain_allowed("ada@notcnrs.fr", &allowed));
        assert!(!email_domain_allowed("ada@cnrs.fr.evil.org", &allowed));
        assert!(email_domain_allowed("ada@anything.org", &[]));
    }
}

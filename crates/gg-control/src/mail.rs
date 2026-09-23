//! Outgoing e-mail, used only to send sign-in links to researchers.
//!
//! The SMTP password comes from an environment variable named in the configuration, never from a
//! file. The simulated mailer (dev_mode only, refused on public deployments) keeps messages in
//! memory; the portal shows them at `/portal/dev/mailbox`.

use std::sync::Mutex;
use std::time::Duration;

use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use zeroize::Zeroizing;

use crate::config::EmailLoginConfig;
use crate::error::ApiError;

/// A message the simulated mailer kept.
#[derive(Clone, Debug)]
pub struct SentMail {
    /// Recipient.
    pub to: String,
    /// Subject.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
}

/// How messages leave the server.
pub enum Mailer {
    /// No e-mail sign-in.
    Disabled,
    /// A real SMTP relay.
    Smtp {
        /// Transport (connection pool, TLS as configured).
        transport: Box<AsyncSmtpTransport<Tokio1Executor>>,
        /// Sender.
        from: Mailbox,
    },
    /// Development only: messages kept in memory, newest last (at most 100).
    Simulated {
        /// Kept messages.
        sent: Mutex<Vec<SentMail>>,
    },
}

impl std::fmt::Debug for Mailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mailer::Disabled => write!(f, "Mailer::Disabled"),
            Mailer::Smtp { from, .. } => write!(f, "Mailer::Smtp(from {from})"),
            Mailer::Simulated { .. } => write!(f, "Mailer::Simulated"),
        }
    }
}

impl Mailer {
    /// Build from a validated configuration.
    pub fn from_config(cfg: &EmailLoginConfig) -> Result<Self, String> {
        match cfg.provider.as_str() {
            "disabled" => Ok(Mailer::Disabled),
            "simulated" => Ok(Mailer::Simulated {
                sent: Mutex::new(Vec::new()),
            }),
            "smtp" => {
                let host = cfg.smtp_host.trim();
                let builder = match cfg.smtp_tls.as_str() {
                    "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                        .map_err(|e| format!("email_login: {e}"))?,
                    "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                        .map_err(|e| format!("email_login: {e}"))?,
                    // Validated: only to a relay on this machine.
                    _ => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
                };
                let mut builder = builder
                    .port(cfg.smtp_port)
                    .timeout(Some(Duration::from_secs(20)));
                if !cfg.smtp_username.is_empty() {
                    let password = Zeroizing::new(
                        std::env::var(&cfg.smtp_password_env).map_err(|_| {
                            format!(
                                "email_login: the SMTP password is expected in the environment variable {}",
                                cfg.smtp_password_env
                            )
                        })?,
                    );
                    builder = builder.credentials(Credentials::new(
                        cfg.smtp_username.clone(),
                        password.to_string(),
                    ));
                }
                let from: Mailbox = cfg
                    .from
                    .parse()
                    .map_err(|e| format!("email_login.from: {e}"))?;
                Ok(Mailer::Smtp {
                    transport: Box::new(builder.build()),
                    from,
                })
            }
            other => Err(format!("unknown email_login.provider '{other}'")),
        }
    }

    /// Whether e-mail sign-in is offered.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Mailer::Disabled)
    }

    /// Whether messages are only simulated.
    pub fn is_simulated(&self) -> bool {
        matches!(self, Mailer::Simulated { .. })
    }

    /// Send a plain-text message.
    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), ApiError> {
        match self {
            Mailer::Disabled => Err(ApiError::NotFound(
                "e-mail sign-in is not configured".into(),
            )),
            Mailer::Simulated { sent } => {
                let mut sent = sent.lock().unwrap_or_else(|e| e.into_inner());
                if sent.len() >= 100 {
                    sent.remove(0);
                }
                sent.push(SentMail {
                    to: to.to_owned(),
                    subject: subject.to_owned(),
                    body: body.to_owned(),
                });
                Ok(())
            }
            Mailer::Smtp { transport, from } => {
                let to: Mailbox = to
                    .parse()
                    .map_err(|_| ApiError::Unprocessable("invalid e-mail address".into()))?;
                let message = Message::builder()
                    .from(from.clone())
                    .to(to)
                    .subject(subject)
                    .header(ContentType::TEXT_PLAIN)
                    .body(body.to_owned())
                    .map_err(|e| ApiError::Internal(format!("building the message: {e}")))?;
                transport.send(message).await.map_err(|e| {
                    tracing::warn!(error = %e, "sending a sign-in e-mail failed");
                    ApiError::Retryable("the e-mail could not be sent, try again later".into())
                })?;
                Ok(())
            }
        }
    }

    /// Messages kept by the simulated mailer (empty otherwise).
    pub fn simulated_outbox(&self) -> Vec<SentMail> {
        match self {
            Mailer::Simulated { sent } => sent.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            _ => Vec::new(),
        }
    }
}

//! Server assembly: middleware, background loops, graceful shutdown.

use std::net::SocketAddr;

use axum::Router;
use tower_http::trace::TraceLayer;

use crate::api;
use crate::config::ServerConfig;
use crate::state::AppState;

/// Build the full application router.
pub fn build_app(state: AppState) -> Router {
    api::router(state.clone())
        .merge(crate::portal::router(state.clone()))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::quota::layer,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::client_ip::layer,
        ))
        .layer(axum::middleware::from_fn_with_state(state, timeout_layer))
        .layer(TraceLayer::new_for_http())
}

/// Bound how long a request may take: `upload_timeout_secs` for an artifact upload (its body is
/// read whole before the answer, so this bounds the slowest uplink accepted), `request_timeout_secs`
/// for everything else. Downloads stream after their headers and are not cut by this bound.
async fn timeout_layer(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = req.uri().path();
    let upload = req.method() == axum::http::Method::PUT
        && (path.contains("/artifacts/") || path.contains("/outputs/"));
    let secs = if upload {
        state.config().upload_timeout_secs
    } else {
        state.config().request_timeout_secs
    };
    match tokio::time::timeout(std::time::Duration::from_secs(secs), next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => axum::http::StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

/// Resolves on the first of Ctrl-C (SIGINT) and, on Unix, SIGTERM (what systemd sends to stop a
/// service).
async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler unavailable; stopping on Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// A running server (used by tests and the demo).
pub struct Running {
    /// Bound address.
    pub addr: SocketAddr,
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Running {
    /// Base URL.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop the server and background loops.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.handle.await;
    }
}

/// Serve on `listener` with background loops; returns immediately.
pub async fn serve(state: AppState, listener: tokio::net::TcpListener) -> Result<Running, String> {
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    let app = build_app(state.clone());
    let reaper = tokio::spawn(crate::workflows::reaper::run(state.clone(), rx.clone()));
    let outbox = tokio::spawn(crate::workflows::outbox::run(state.clone(), rx.clone()));
    let coordinator = tokio::spawn(crate::workflows::collective::run(state.clone(), rx.clone()));
    let retention = tokio::spawn(crate::workflows::retention::run(state.clone(), rx.clone()));
    let mut rx_http = rx.clone();
    let handle = tokio::spawn(async move {
        // TCP_NODELAY: without it, a response's header and body leave in two segments and the
        // second waits for the client's delayed acknowledgement (Nagle's algorithm): 20 to 40 ms
        // per download, even for 1 KB, measured on every input of a verification.
        use axum::serve::ListenerExt;
        let listener = listener.tap_io(|tcp| {
            if let Err(e) = tcp.set_nodelay(true) {
                tracing::debug!(error = %e, "TCP_NODELAY refused");
            }
        });
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = rx_http.wait_for(|v| *v).await;
        });
        if let Err(e) = server.await {
            tracing::error!(error = %e, "http server stopped with error");
        }
        let _ = reaper.await;
        let _ = outbox.await;
        let _ = coordinator.await;
        let _ = retention.await;
    });
    tracing::info!(%addr, "control plane listening");
    Ok(Running {
        addr,
        shutdown: tx,
        handle,
    })
}

/// Connect, migrate, build state and serve until Ctrl-C.
pub async fn run(config: ServerConfig) -> Result<(), String> {
    if config.public && config.dev_mode {
        return Err("refusing to start: dev_mode on a public deployment".into());
    }
    if config.dev_mode {
        tracing::warn!("DEV MODE is enabled: not for public deployments");
    }
    let db = gg_storage::Db::connect(&config.database_url, config.pool_size)
        .await
        .map_err(|e| e.to_string())?;
    db.migrate().await.map_err(|e| e.to_string())?;
    let bind = config.bind_addr.clone();
    let state = AppState::new(config, db).await?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("bind {bind}: {e}"))?;
    let running = serve(state, listener).await?;
    terminated().await;
    tracing::info!("shutdown requested");
    running.stop().await;
    Ok(())
}

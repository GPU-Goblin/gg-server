//! HTTP API `/v1`: researcher, worker, validator and admin route groups.

pub mod admin;
pub mod common;
pub mod researcher;
pub mod validator;
pub mod worker;

use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::state::AppState;

/// OpenAPI document root.
#[derive(OpenApi)]
#[openapi(
    info(title = "GPU Goblin control API", version = "1", description = "Versioned API of the GPU Goblin control plane. Signed protocol messages are documented in https://github.com/GPU-Goblin/gg-core/blob/main/docs/PROTOCOL.md."),
    tags(
        (name = "common", description = "Health and discovery"),
        (name = "researcher", description = "Researcher routes (API token, project membership required)"),
        (name = "worker", description = "Worker routes (challenge-based session)"),
        (name = "validator", description = "Validator routes (administered identity)"),
        (name = "admin", description = "Operator routes")
    )
)]
pub struct ApiDoc;

/// Build the `/v1` router and the OpenAPI document.
pub fn router(state: AppState) -> Router {
    let json_limit = state.config().max_json_bytes;
    let upload_limit = state.config().max_upload_bytes as usize + 1024;

    let (json_routes, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(common::routes())
        .merge(researcher::json_routes())
        .merge(worker::json_routes())
        .merge(validator::json_routes())
        .merge(admin::routes())
        .split_for_parts();
    let json_routes = json_routes.layer(RequestBodyLimitLayer::new(json_limit));

    let upload_routes = Router::new()
        .merge(researcher::upload_routes())
        .merge(worker::upload_routes())
        .merge(validator::upload_routes())
        .layer(RequestBodyLimitLayer::new(upload_limit));

    let doc = std::sync::Arc::new(api);
    let openapi = Router::new().route(
        "/openapi.json",
        axum::routing::get(move || {
            let doc = doc.clone();
            async move { axum::Json((*doc).clone()) }
        }),
    );

    Router::new()
        .nest("/v1", json_routes.merge(upload_routes).merge(openapi))
        .with_state(state)
}

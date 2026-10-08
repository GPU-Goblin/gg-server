//! Health and discovery.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::state::AppState;

/// Health response.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Health {
    /// Always `ok` when the process answers.
    pub status: &'static str,
    /// Database reachable.
    pub database: bool,
    /// Uptime in seconds.
    pub uptime_secs: u64,
}

/// Discovery info.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Info {
    /// Network id.
    pub network_id: String,
    /// Protocol version.
    pub protocol_version: u32,
    /// Catalog version.
    pub catalog_version: u32,
    /// Compute kinds.
    pub compute_kinds: Vec<&'static str>,
    /// Server signing public key (hex).
    pub server_public_key: String,
    /// Development mode flag (never true on public deployments).
    pub dev_mode: bool,
    /// Identifier of the rule about what data may be submitted here.
    pub data_policy: String,
    /// That rule in plain words, so a submitter reads it before uploading anything.
    pub data_policy_text: String,
    /// Size of the chunks a peer manifest describes, when peer distribution is on; `0` otherwise.
    /// A worker declaring an activation before its durable copy computes its chunk digests with it
    /// (ADR-0026).
    pub p2p_chunk_bytes: u32,
    /// Largest output of a job a lab node holds that the control plane still accepts as an upload
    /// (ADR-0041); larger ones are declared and pulled by the node.
    pub node_relay_max_bytes: u64,
    /// How long a declared output waits for its holder before its task is sent again: a worker
    /// keeps serving it that long.
    pub pending_output_secs: i64,
}

/// One operator of the published profile (ADR-0016).
#[derive(Serialize, utoipa::ToSchema)]
pub struct OpEntry {
    /// Name used in a graph description.
    pub name: String,
    /// Closest ONNX operator and opset, when there is one.
    pub onnx: Option<String>,
    /// Minimum number of inputs.
    pub min_inputs: usize,
    /// Maximum number of inputs.
    pub max_inputs: usize,
    /// Recognised attributes. Anything else is refused, never ignored.
    pub attributes: Vec<String>,
    /// Attributes that must be given.
    pub required: Vec<String>,
    /// What the operator guarantees from one run to the next.
    pub determinism: String,
    /// One sentence about what it does.
    pub summary: String,
}

/// The operator profile a graph description is checked against.
#[derive(Serialize, utoipa::ToSchema)]
pub struct OpProfile {
    /// Profile version, quoted in every refusal message.
    pub profile: String,
    /// Graph description format.
    pub format: String,
    /// Maximum number of nodes.
    pub max_nodes: usize,
    /// Maximum number of trainable parameters.
    pub max_parameters: u64,
    /// Maximum tensor rank.
    pub max_rank: usize,
    /// The operators themselves.
    pub operators: Vec<OpEntry>,
}

/// Plain-words version of a data policy identifier.
pub fn data_policy_text(id: &str) -> String {
    match id {
        "public-synthetic-only" => {
            "Only synthetic or already public data may be submitted. The owner of \
the machine doing the computation sees the data it processes: the platform offers no \
confidentiality with respect to contributors."
                .to_owned()
        }
        "research-any-data" => {
            "Any kind of research data may be submitted. Be careful with confidentiality: the \
owner of the machine doing the computation sees the data it processes, and the platform offers \
no confidentiality with respect to contributors. Personal data (voices, health records, ...) \
remain the responsibility of the researcher and of their institution's data protection officer."
                .to_owned()
        }
        other => format!("Rule specific to this deployment: {other}. Ask the operator."),
    }
}

#[utoipa::path(get, path = "/health", tag = "common", responses((status = 200, body = Health)))]
async fn health(State(state): State<AppState>) -> Json<Health> {
    let database = sqlx::query("SELECT 1")
        .execute(state.db().pool())
        .await
        .is_ok();
    Json(Health {
        status: "ok",
        database,
        uptime_secs: state.uptime_secs(),
    })
}

#[utoipa::path(get, path = "/info", tag = "common", responses((status = 200, body = Info)))]
async fn info(State(state): State<AppState>) -> Json<Info> {
    Json(Info {
        network_id: state.network().to_string(),
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        catalog_version: gg_compute::CATALOG_VERSION,
        compute_kinds: gg_compute::catalog::KINDS.to_vec(),
        server_public_key: state.server_key().public_key().to_hex(),
        dev_mode: state.config().dev_mode,
        data_policy: state.config().data_policy.clone(),
        data_policy_text: data_policy_text(&state.config().data_policy),
        p2p_chunk_bytes: if state.config().p2p_enabled {
            state.config().p2p_chunk_bytes
        } else {
            0
        },
        node_relay_max_bytes: state.config().node_relay_max_bytes,
        pending_output_secs: state.config().pending_output_secs,
    })
}

#[utoipa::path(get, path = "/catalog/ops", tag = "common", responses((status = 200, body = OpProfile)))]
async fn catalog_ops() -> Json<OpProfile> {
    use gg_compute::graph;
    Json(OpProfile {
        profile: graph::PROFILE_VERSION.to_owned(),
        format: graph::FORMAT.to_owned(),
        max_nodes: graph::bounds::MAX_NODES,
        max_parameters: graph::bounds::MAX_PARAMETERS,
        max_rank: graph::bounds::MAX_RANK,
        operators: graph::profile()
            .into_iter()
            .map(|spec| OpEntry {
                name: spec.name.to_owned(),
                onnx: spec.onnx.map(ToOwned::to_owned),
                min_inputs: spec.arity.0,
                max_inputs: spec.arity.1,
                attributes: spec.attributes.iter().map(|a| (*a).to_owned()).collect(),
                required: spec.required.iter().map(|a| (*a).to_owned()).collect(),
                determinism: match spec.determinism {
                    graph::Determinism::Bitwise => "bitwise",
                    graph::Determinism::Tolerance => "tolerance",
                    graph::Determinism::Seeded => "seeded",
                }
                .to_owned(),
                summary: spec.summary.to_owned(),
            })
            .collect(),
    })
}

/// Routes.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(info))
        .routes(routes!(catalog_ops))
}

#[cfg(test)]
mod tests {
    use super::data_policy_text;

    #[test]
    fn each_known_data_rule_says_who_sees_the_data() {
        for id in ["public-synthetic-only", "research-any-data"] {
            let text = data_policy_text(id);
            assert!(text.contains("sees the data it processes"), "{id}: {text}");
        }
        assert!(data_policy_text("research-any-data").contains("data protection officer"));
        assert!(data_policy_text("custom").contains("Ask the operator"));
    }
}

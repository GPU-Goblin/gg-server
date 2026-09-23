//! Control plane of GPU Goblin: HTTP API (`/v1`), scheduler and transactional workflows.

#![forbid(unsafe_code)]

pub mod api;
pub mod auth;
pub mod client_ip;
pub mod config;
pub mod error;
pub mod mail;
pub mod orcid;
pub mod portal;
pub mod quota;
pub mod server;
pub mod state;
pub mod workflows;

pub use config::ServerConfig;
pub use error::ApiError;
pub use state::AppState;

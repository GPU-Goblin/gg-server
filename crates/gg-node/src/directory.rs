//! The list of control planes a node may work with (ADR-0041, A7): published at a fixed address on
//! gpugoblin.com, signed offline with the release key (SSHSIG, namespace
//! [`NAMESPACE`]), the same key and tool (`ssh-keygen -Y sign`) as the worker's releases. A node
//! refuses a list that is unsigned, signed by a key it does not trust, expired, or older than one it
//! already accepted; and it later checks that each server presents the key the list names.

use std::path::Path;

use anyhow::{Context, bail};
use gg_core::UnixMillis;
use gg_protocol::keys::PublicKey;
use serde::{Deserialize, Serialize};

/// SSHSIG namespace of the list: a release signature cannot be replayed as a list, nor the reverse.
pub const NAMESPACE: &str = "gpugoblin-servers";

/// Format label.
pub const FORMAT: &str = "gg-servers/v1";

/// One control plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// Shown to people.
    #[serde(default)]
    pub name: String,
    /// Base URL (`https://…`).
    pub url: String,
    /// Network identifier the server signs with.
    pub network: String,
    /// The server's signing key (hex): what its `/v1/info` must present.
    pub server_public_key: String,
}

/// The signed list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerList {
    pub format: String,
    /// When it was signed; a node never goes back to an older list.
    pub issued_at: UnixMillis,
    /// Past this, the list is refused.
    pub expires_at: UnixMillis,
    pub servers: Vec<ServerEntry>,
}

/// Check a list and its armored SSHSIG signature against the trusted keys, at time `now`, and
/// against the `issued_at` of the last list accepted (rollback).
pub fn verify(
    list_bytes: &[u8],
    signature: &str,
    trusted: &[PublicKey],
    now: UnixMillis,
    last_accepted: Option<UnixMillis>,
    allow_plain_http: bool,
) -> anyhow::Result<ServerList> {
    let signer = gg_protocol::release::sshsig_verify(signature, NAMESPACE, list_bytes)
        .map_err(|e| anyhow::anyhow!("server list signature: {e}"))?;
    if !trusted.contains(&signer) {
        bail!("server list signed by a key this node does not trust");
    }
    let list: ServerList = serde_json::from_slice(list_bytes).context("server list")?;
    if list.format != FORMAT {
        bail!("server list format '{}', expected {FORMAT}", list.format);
    }
    if list.expires_at.0 <= now.0 {
        bail!("server list expired");
    }
    if let Some(last) = last_accepted
        && list.issued_at.0 < last.0
    {
        bail!("server list older than one already accepted");
    }
    if list.servers.is_empty() || list.servers.len() > 64 {
        bail!("server list must name 1 to 64 servers");
    }
    for s in &list.servers {
        let served_well =
            s.url.starts_with("https://") || (allow_plain_http && s.url.starts_with("http://"));
        if !served_well {
            bail!("server {} is not served over https", s.url);
        }
        s.server_public_key
            .parse::<PublicKey>()
            .map_err(|e| anyhow::anyhow!("server {}: key: {e}", s.url))?;
        gg_core::NetworkId::new(&s.network)
            .map_err(|e| anyhow::anyhow!("server {}: network: {e}", s.url))?;
    }
    Ok(list)
}

/// Fetch the list at `url` and its signature at `url` + `.sig`, verify them, and remember the
/// accepted `issued_at` in `state_dir`.
pub async fn fetch(
    url: &str,
    trusted: &[PublicKey],
    state_dir: &Path,
    allow_plain_http: bool,
) -> anyhow::Result<ServerList> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let get = |u: String| {
        let http = http.clone();
        async move {
            let r = http.get(&u).send().await?.error_for_status()?;
            // A list is small: anything larger is refused before it is read in full.
            if r.content_length().is_some_and(|n| n > 1 << 20) {
                bail!("{u}: too large");
            }
            let bytes = r.bytes().await?;
            if bytes.len() > 1 << 20 {
                bail!("{u}: too large");
            }
            anyhow::Ok(bytes.to_vec())
        }
    };
    let list = get(url.to_owned()).await.context("fetch the server list")?;
    let sig = get(format!("{url}.sig"))
        .await
        .context("fetch the server list signature")?;
    let seen = state_dir.join("servers.issued_at");
    let last = std::fs::read_to_string(&seen)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .map(UnixMillis);
    let accepted = verify(
        &list,
        std::str::from_utf8(&sig).context("signature is not text")?,
        trusted,
        UnixMillis::now(),
        last,
        allow_plain_http,
    )?;
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(&seen, accepted.issued_at.0.to_string())?;
    Ok(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gg_protocol::keys::SecretKey;
    use gg_protocol::release::sshsig_sign;

    fn list(issued: i64, expires: i64, url: &str) -> Vec<u8> {
        let key = SecretKey::generate().unwrap().public_key();
        serde_json::to_vec(&ServerList {
            format: FORMAT.into(),
            issued_at: UnixMillis(issued),
            expires_at: UnixMillis(expires),
            servers: vec![ServerEntry {
                name: "beta".into(),
                url: url.into(),
                network: "gg-beta".into(),
                server_public_key: key.to_hex(),
            }],
        })
        .unwrap()
    }

    #[test]
    fn only_a_fresh_list_signed_by_a_trusted_key_is_accepted() {
        let release = SecretKey::generate().unwrap();
        let other = SecretKey::generate().unwrap();
        let trusted = [release.public_key()];
        let now = UnixMillis(1_000);
        let good = list(900, 2_000, "https://grid.gpugoblin.com");
        let sig = sshsig_sign(&release, NAMESPACE, &good);
        assert!(verify(&good, &sig, &trusted, now, None, false).is_ok());
        // Another signer, another namespace, altered bytes.
        let e = verify(
            &good,
            &sshsig_sign(&other, NAMESPACE, &good),
            &trusted,
            now,
            None,
            false,
        );
        assert!(e.unwrap_err().to_string().contains("does not trust"));
        let e = verify(
            &good,
            &sshsig_sign(&release, "gpugoblin-release", &good),
            &trusted,
            now,
            None,
            false,
        );
        assert!(e.is_err());
        let mut altered = good.clone();
        let n = altered.len();
        altered[n - 3] ^= 1;
        assert!(verify(&altered, &sig, &trusted, now, None, false).is_err());
        // Expired, or older than the last list accepted.
        let old = list(900, 999, "https://grid.gpugoblin.com");
        let e = verify(
            &old,
            &sshsig_sign(&release, NAMESPACE, &old),
            &trusted,
            now,
            None,
            false,
        );
        assert!(e.unwrap_err().to_string().contains("expired"));
        let e = verify(&good, &sig, &trusted, now, Some(UnixMillis(950)), false);
        assert!(e.unwrap_err().to_string().contains("older"));
        // Plain http only when allowed (development).
        let plain = list(900, 2_000, "http://10.0.0.2:8080");
        let psig = sshsig_sign(&release, NAMESPACE, &plain);
        assert!(verify(&plain, &psig, &trusted, now, None, false).is_err());
        assert!(verify(&plain, &psig, &trusted, now, None, true).is_ok());
    }
}

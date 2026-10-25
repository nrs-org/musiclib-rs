//! Keeps the MusicBrainz URL mirror (`mb_mirror.db`) consistent with the
//! server the MusicBrainz backend queries, and checks that server is current.
//!
//! The mirror decides which URLs are worth asking the server about. A mirror
//! ahead of a self-hosted server vouches for URLs the server doesn't have
//! yet: the lookup 404s, and the 404 is cached, so the link is never found.
//! So the mirror may hold at most the server's replication sequence. The
//! public server is always ahead of any mirror, so it needs no check.
//!
//! Everything here blocks (child processes, `reqwest::blocking`); call it
//! from `spawn_blocking` in async code.

use std::{path::Path, process::Command, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::providers::registry::MusicBrainzConfig;

/// How to read a self-hosted server's replication sequence: `{command: …}`
/// or `{url: …}` in providers.yaml.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ServerReplication {
    /// A shell command printing the sequence, e.g. a `psql` query against the
    /// `replication_control` table in musicbrainz-docker's database container.
    Command { command: String },
    /// A URL serving JSON with a `replication_sequence` field, e.g.
    /// lightweight-mb's `/healthz`.
    Url { url: String },
}

/// Where MetaBrainz publishes replication packets and their index.
pub const PACKETS_BASE_URL: &str = "https://metabrainz.org/api/musicbrainz";

const TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `base_url` (the backend's, `None` for the default) is the public
/// musicbrainz.org server.
pub fn is_public_server(base_url: Option<&str>) -> bool {
    let Some(url) = base_url else { return true };
    match reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    {
        Some(host) => host == "musicbrainz.org" || host.ends_with(".musicbrainz.org"),
        None => false,
    }
}

/// The server's current replication sequence.
pub fn server_sequence(src: &ServerReplication) -> Result<i64> {
    match src {
        ServerReplication::Command { command: cmd } => {
            let out = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .with_context(|| format!("running `{cmd}`"))?;
            if !out.status.success() {
                bail!(
                    "`{cmd}` failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            parse_sequence_output(&String::from_utf8_lossy(&out.stdout))
                .with_context(|| format!("reading the output of `{cmd}`"))
        }
        ServerReplication::Url { url } => {
            let body = http_client()?
                .get(url)
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .with_context(|| format!("requesting {url}"))?
                .text()?;
            parse_healthz(&body).with_context(|| format!("reading {url}"))
        }
    }
}

/// The newest packet MetaBrainz has published.
pub fn latest_published(packets_base_url: &str, token: &str) -> Result<i64> {
    let url = format!(
        "{}/replication-info?token={}",
        packets_base_url.trim_end_matches('/'),
        urlencoding::encode(token)
    );
    let body = http_client()?
        .get(&url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .context("requesting the replication index")?
        .text()?;
    parse_replication_info(&body)
}

/// The mirror's replication sequence.
pub fn mirror_sequence(path: &Path) -> Result<i64> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", path.display()))?;
    conn.query_row("SELECT replication_sequence FROM mb_state", [], |row| {
        row.get(0)
    })
    .with_context(|| format!("reading mb_state in {}", path.display()))
}

/// The highest packet the mirror may hold: the server's sequence, or `None`
/// (no limit) for the public server. A self-hosted server without
/// `server_replication` can't be checked, which is an error.
pub fn mirror_cap(cfg: &MusicBrainzConfig) -> Result<Option<i64>> {
    let base_url = cfg.resolved_base_url();
    if is_public_server(base_url.as_deref()) {
        return Ok(None);
    }
    let Some(src) = &cfg.server_replication else {
        bail!(
            "MusicBrainz server {} is self-hosted, so the mirror must not get ahead \
             of it, but there's no `musicbrainz.server_replication` in providers.yaml \
             to read its replication sequence (see docs/CONFIG_REFERENCE.md)",
            base_url.unwrap_or_default()
        );
    };
    server_sequence(src)
        .map(Some)
        .context("reading the MusicBrainz server's replication sequence")
}

/// Refuse a run whose mirror is ahead of its self-hosted server (e.g. after
/// restoring an older server backup). `Ok(None)` when there's nothing to
/// check (no mirror, or the public server); otherwise the two sequences.
pub fn check_mirror_not_ahead(cfg: &MusicBrainzConfig) -> Result<Option<(i64, i64)>> {
    let Some(path) = cfg.mirror_db_path() else {
        return Ok(None);
    };
    let Some(cap) = mirror_cap(cfg)? else {
        return Ok(None);
    };
    let mirror = mirror_sequence(&path)?;
    if mirror > cap {
        bail!(
            "the MusicBrainz URL mirror ({}) is at packet {mirror}, ahead of the server's \
             {cap}: its extra links would be looked up and 404 into the cache. Wait for the \
             server to replicate, or undo the extra packets with \
             `mb_apply_replication --undo`",
            path.display()
        );
    }
    Ok(Some((mirror, cap)))
}

/// The check `import` runs before a walk: a mirror ahead of its server
/// refuses the run. A self-hosted server that can't be read only warns: the
/// mirror can't have moved past it through `mb_sync_replication` or
/// `mb_apply_replication`, which both refuse without `server_replication`.
pub fn check_before_import(cfg: &MusicBrainzConfig) -> Result<()> {
    if cfg.mirror_db_path().is_none() || is_public_server(cfg.resolved_base_url().as_deref()) {
        return Ok(());
    }
    if cfg.server_replication.is_none() {
        tracing::warn!(
            "can't check that the MusicBrainz URL mirror isn't ahead of the self-hosted \
             server: set `musicbrainz.server_replication` in providers.yaml"
        );
        return Ok(());
    }
    if let Some((mirror, server)) = check_mirror_not_ahead(cfg)? {
        tracing::info!("MusicBrainz URL mirror at packet {mirror}, server at {server}");
    }
    Ok(())
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("musiclib-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

fn parse_sequence_output(out: &str) -> Result<i64> {
    let out = out.trim();
    out.parse()
        .with_context(|| format!("expected a replication sequence, got {out:?}"))
}

fn parse_healthz(body: &str) -> Result<i64> {
    let v: serde_json::Value = serde_json::from_str(body).context("parsing JSON")?;
    v["replication_sequence"]
        .as_i64()
        .context("no integer `replication_sequence` field")
}

/// `{"last_packet":"replication-190007.tar.bz2"}` → 190007.
fn parse_replication_info(body: &str) -> Result<i64> {
    let v: serde_json::Value = serde_json::from_str(body).context("parsing JSON")?;
    let name = v["last_packet"]
        .as_str()
        .context("no `last_packet` in the replication index")?;
    let digits: String = name
        .strip_prefix("replication-")
        .unwrap_or_default()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .with_context(|| format!("unexpected packet name {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example providers file parses, `server_replication` included.
    #[test]
    fn example_config_has_server_replication() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/providers.example.yaml");
        let cfg = crate::providers::registry::RegistryConfig::load(Some(Path::new(path))).unwrap();
        let mb = cfg.musicbrainz.unwrap();
        assert!(matches!(
            mb.server_replication,
            Some(ServerReplication::Command { ref command }) if command.contains("replication_control")
        ));
    }

    #[test]
    fn url_form_parses() {
        let r: ServerReplication =
            serde_yaml_ng::from_str("url: http://localhost:8080/healthz").unwrap();
        assert!(matches!(r, ServerReplication::Url { ref url } if url.ends_with("/healthz")));
    }

    #[test]
    fn public_server_detection() {
        assert!(is_public_server(None));
        assert!(is_public_server(Some("https://musicbrainz.org/ws/2")));
        assert!(is_public_server(Some("https://beta.musicbrainz.org/ws/2")));
        assert!(!is_public_server(Some("http://localhost:5000/ws/2")));
        assert!(!is_public_server(Some("http://notmusicbrainz.org/ws/2")));
    }

    #[test]
    fn parses_sequences() {
        assert_eq!(parse_sequence_output(" 189999\n").unwrap(), 189999);
        assert!(parse_sequence_output("").is_err());
        assert_eq!(
            parse_healthz(r#"{"schema_sequence":31,"replication_sequence":190001,"ready":true}"#)
                .unwrap(),
            190001
        );
        assert!(parse_healthz(r#"{"ready":true}"#).is_err());
        assert_eq!(
            parse_replication_info(r#"{"last_packet":"replication-190007.tar.bz2"}"#).unwrap(),
            190007
        );
        assert_eq!(
            parse_replication_info(r#"{"last_packet":"replication-190007-v2.tar.bz2"}"#).unwrap(),
            190007
        );
        assert!(parse_replication_info(r#"{"last_packet":"other"}"#).is_err());
    }

    #[test]
    fn server_sequence_from_command() {
        let ok = ServerReplication::Command {
            command: "echo 42".into(),
        };
        assert_eq!(server_sequence(&ok).unwrap(), 42);
        let failing = ServerReplication::Command {
            command: "echo nope >&2; exit 3".into(),
        };
        let err = format!("{:#}", server_sequence(&failing).unwrap_err());
        assert!(err.contains("nope"), "{err}");
    }

    fn self_hosted(server_replication: Option<ServerReplication>) -> MusicBrainzConfig {
        MusicBrainzConfig {
            base_url: Some(crate::providers::registry::Credential::Value(
                "http://localhost:5000/ws/2".into(),
            )),
            server_replication,
            ..Default::default()
        }
    }

    #[test]
    fn mirror_cap_by_server_kind() {
        let public = MusicBrainzConfig {
            base_url: Some(crate::providers::registry::Credential::Value(
                "https://musicbrainz.org/ws/2".into(),
            )),
            ..Default::default()
        };
        assert_eq!(mirror_cap(&public).unwrap(), None);
        assert!(mirror_cap(&self_hosted(None)).is_err());
        let cfg = self_hosted(Some(ServerReplication::Command {
            command: "echo 100".into(),
        }));
        assert_eq!(mirror_cap(&cfg).unwrap(), Some(100));
    }

    /// A mirror ahead of its self-hosted server stops the run; one at or
    /// behind it passes.
    #[test]
    fn mirror_ahead_of_server_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mb_mirror.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE mb_state(id INTEGER PRIMARY KEY, replication_sequence INTEGER, \
             schema_sequence INTEGER); INSERT INTO mb_state VALUES (1, 100, 31);",
        )
        .unwrap();
        drop(conn);
        let with_server = |seq: i64| MusicBrainzConfig {
            mirror_db: Some(crate::providers::registry::Credential::Value(
                path.display().to_string(),
            )),
            ..self_hosted(Some(ServerReplication::Command {
                command: format!("echo {seq}"),
            }))
        };
        assert_eq!(
            check_mirror_not_ahead(&with_server(100)).unwrap(),
            Some((100, 100))
        );
        assert!(check_mirror_not_ahead(&with_server(99)).is_err());
    }
}

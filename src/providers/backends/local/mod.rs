mod types;

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use async_trait::async_trait;

use crate::providers::{
    CanonicalizeProvider, FetchProvider, TryDefault,
    matcher::filter_children,
    types::{
        Alias, CachedChildSource, CanonicalizeResult, ChildRef, Contribution, EntityResult,
        EntryFetchOptionsPool, EntrySpecificData, Error, ExternalSources, OptionsId, TrackPosition,
    },
};

use types::{LocalChild, LocalEntry};

pub const SOURCE: &str = "local";
pub const EXTERNAL_TYPE_ENTRY: &str = "local:entry";

pub struct Provider {
    base_dir: std::path::PathBuf,
}

impl Provider {
    pub fn new(base_dir: std::path::PathBuf) -> Self {
        Self { base_dir }
    }
}

impl TryDefault for Provider {
    type Error = Error;

    fn try_default() -> Result<Self, Error> {
        let base_dir = std::env::current_dir()
            .map_err(|e| Error::InvalidUrl(format!("cannot determine CWD: {e}")))?;
        Ok(Self { base_dir })
    }
}

/// Parse a `local://` URL into a filesystem path.
///
/// - `local:///absolute/path.yaml` → `/absolute/path.yaml`
/// - `local://relative/path.yaml`  → `relative/path.yaml` (relative to CWD)
pub fn parse_local_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("local://")?;
    Some(PathBuf::from(rest))
}

pub fn to_local_url(path: &std::path::Path) -> String {
    format!("local://{}", path.display())
}

fn match_local_url(url: &str, base_dir: &std::path::Path) -> Option<PathBuf> {
    let path = parse_local_url(url)?;
    if path.is_absolute() {
        Some(path)
    } else {
        Some(base_dir.join(path))
    }
}

fn read_entry(path: &std::path::Path) -> Result<LocalEntry, Error> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Error::InvalidUrl(format!("cannot read {}: {e}", path.display())))?;
    serde_yaml_ng::from_str(&content)
        .map_err(|e| Error::InvalidUrl(format!("cannot parse {}: {e}", path.display())))
}

fn build_sources(raw: &std::collections::HashMap<String, Vec<String>>) -> ExternalSources {
    let mut sources = ExternalSources::default();
    for (k, vs) in raw {
        sources
            .0
            .entry(k.clone().into())
            .or_default()
            .extend(vs.iter().cloned());
    }
    sources
}

fn build_child_ref(child: &LocalChild, base: &std::path::Path) -> ChildRef {
    let mut sources = ExternalSources::default();
    for (k, vs) in &child.sources {
        let resolved: HashSet<String> = vs
            .iter()
            .map(|v| {
                // Resolve relative paths and local:// paths relative to the
                // base file's directory.
                let rel = v.strip_prefix("local://").unwrap_or(v.as_str());
                if rel.starts_with('/') || rel.starts_with('.') || !v.contains("://") {
                    to_local_url(&base.join(rel))
                } else {
                    v.clone()
                }
            })
            .collect();
        sources
            .0
            .entry(k.clone().into())
            .or_default()
            .extend(resolved);
    }

    let contributions = child
        .contributions
        .iter()
        .map(|c| Contribution {
            role: c.role.clone(),
            main_artist: c.main_artist,
            source: SOURCE.into(),
            extra: serde_json::Value::Null,
        })
        .collect();

    let position = child.position.as_ref().map(|p| TrackPosition {
        track_no: p.track_no,
        disc_no: p.disc_no,
    });

    ChildRef {
        entry_type: child.entry_type,
        external_type: EXTERNAL_TYPE_ENTRY.into(),
        sources,
        name: child.name.clone(),
        position,
        contributions,
    }
}

fn entry_to_entity_result(entry: LocalEntry, base: &std::path::Path) -> EntityResult<()> {
    let sources = build_sources(&entry.sources);

    let specific_data = match entry.entry_type {
        crate::providers::types::EntryType::Track => EntrySpecificData::Track {
            duration_ms: entry.duration_ms,
            positions: Default::default(),
        },
        crate::providers::types::EntryType::Release => EntrySpecificData::Release {
            release_type: entry.release_type,
            num_discs: entry.num_discs,
            num_tracks: entry.num_tracks,
        },
        crate::providers::types::EntryType::ReleaseGroup => EntrySpecificData::ReleaseGroup {
            primary_type: entry.primary_type,
        },
        crate::providers::types::EntryType::Artist => EntrySpecificData::Artist,
    };

    let aliases = entry
        .aliases
        .into_iter()
        .map(|a| Alias {
            name: a.name,
            source: SOURCE.into(),
            locale: a.locale,
            primary: a.primary,
            extra: serde_json::Value::Null,
        })
        .collect();

    let children = entry
        .children
        .iter()
        .map(|c| build_child_ref(c, base))
        .collect::<Vec<_>>();

    EntityResult {
        release_date: entry.release_date,
        sources,
        extra: serde_json::Value::Null,
        specific_data,
        children: vec![Arc::new(CachedChildSource::from_children(children))],
        aliases,
    }
}

#[async_trait]
impl CanonicalizeProvider for Provider {
    async fn canonicalize(&self, url: &str) -> Option<CanonicalizeResult> {
        let path = match_local_url(url, &self.base_dir)?;
        // Canonicalize path to absolute to ensure stable identifiers.
        let abs = std::fs::canonicalize(&path).ok()?;
        let entry = read_entry(&abs).ok()?;
        Some(CanonicalizeResult {
            canonical_identifier: to_local_url(&abs),
            entry_type: entry.entry_type,
            external_type: EXTERNAL_TYPE_ENTRY.into(),
        })
    }
}

#[async_trait]
impl FetchProvider for Provider {
    async fn fetch_entry(
        self: Arc<Self>,
        identifier: &str,
        pool: Arc<EntryFetchOptionsPool>,
        root_id: OptionsId,
    ) -> Result<EntityResult, Error> {
        let path = match_local_url(identifier, &self.base_dir)
            .ok_or_else(|| Error::InvalidUrl(identifier.to_string()))?;
        let entry = read_entry(&path)?;
        let base = path.parent().unwrap_or(std::path::Path::new("."));
        let result = entry_to_entity_result(entry, base);

        let children = result
            .children
            .iter()
            .map(|s| {
                Ok(Arc::new(filter_children(
                    s.clone(),
                    pool.clone(),
                    root_id,
                    self.clone(),
                )?))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(EntityResult {
            children,
            release_date: result.release_date,
            sources: result.sources,
            extra: result.extra,
            specific_data: result.specific_data,
            aliases: result.aliases,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::providers::types::{ChildSource, EntrySpecificData, EntryType, child_next};

    use super::{entry_to_entity_result, read_entry, to_local_url};

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/providers/backends/local/fixtures")
            .join(name)
    }

    fn fetch(name: &str) -> crate::providers::types::EntityResult<()> {
        let path = fixture_path(name);
        let entry = read_entry(&path).expect("read_entry failed");
        let base = path.parent().unwrap();
        entry_to_entity_result(entry, base)
    }

    fn fixtures_provider() -> super::Provider {
        super::Provider::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/providers/backends/local/fixtures"),
        )
    }

    #[tokio::test]
    async fn test_local_track() -> anyhow::Result<()> {
        let track = fetch("track.yaml");

        assert!(matches!(
            track.specific_data,
            EntrySpecificData::Track {
                duration_ms: Some(240000),
                ..
            }
        ));
        assert_eq!(track.aliases[0].name, "My Song");
        assert!(track.aliases[0].primary);
        assert_eq!(track.release_date.as_deref(), Some("2024-01-01"));

        assert_eq!(track.children.len(), 1);
        let mut artists = track.children[0].cursor();
        let (artist, _) = child_next(&mut artists).await?.expect("expected artist");
        assert_eq!(artist.entry_type, EntryType::Artist);
        assert_eq!(artist.name.as_deref(), Some("Local Artist"));

        Ok(())
    }

    #[tokio::test]
    async fn test_local_artist() -> anyhow::Result<()> {
        let artist = fetch("artist.yaml");

        assert!(matches!(artist.specific_data, EntrySpecificData::Artist));
        assert_eq!(artist.aliases[0].name, "Local Artist");

        Ok(())
    }

    #[tokio::test]
    async fn test_local_release() -> anyhow::Result<()> {
        let release = fetch("release.yaml");

        assert!(matches!(
            release.specific_data,
            EntrySpecificData::Release {
                num_tracks: Some(2),
                ..
            }
        ));
        assert_eq!(release.aliases[0].name, "Local Album");

        let mut children = release.children[0].cursor();
        let (first, _) = child_next(&mut children)
            .await?
            .expect("expected first child");
        assert_eq!(first.entry_type, EntryType::Artist);

        let (second, _) = child_next(&mut children)
            .await?
            .expect("expected second child");
        assert_eq!(second.entry_type, EntryType::Track);
        assert_eq!(second.position.as_ref().map(|p| p.track_no), Some(1));

        let (third, _) = child_next(&mut children)
            .await?
            .expect("expected third child");
        assert_eq!(third.entry_type, EntryType::Track);
        assert_eq!(third.position.as_ref().map(|p| p.track_no), Some(2));

        Ok(())
    }

    #[tokio::test]
    async fn test_local_url_resolution() -> anyhow::Result<()> {
        let base = fixture_path("release.yaml");
        let base_dir = base.parent().unwrap();
        let entry = read_entry(&base).expect("read_entry failed");
        let result = entry_to_entity_result(entry, base_dir);

        // The local:// child sources should be resolved to absolute paths.
        let mut cursor = result.children[0].cursor();
        let (child, _) = child_next(&mut cursor).await?.expect("expected child");
        let local_ids = child.sources.get("local").unwrap();
        assert!(
            local_ids.iter().next().unwrap().starts_with("local:///"),
            "local child source should be absolute"
        );

        Ok(())
    }
}

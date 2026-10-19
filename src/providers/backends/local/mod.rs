mod types;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;

use crate::providers::{
    CanonicalizeProvider, FetchProvider, TryDefault,
    matcher::filter_children,
    types::{
        Alias, CachedChildSource, CanonicalizeResult, ChildRef, Contribution, EntityResult,
        EntryFetchOptionsPool, EntrySpecificData, Error, ExternalSources, OptionsId, TrackPosition,
    },
};

use crate::providers::registry::canonicalize;
use crate::providers::std_values::StandardProviderKeys;
use types::{LocalChild, LocalEntry, NamespaceConfig};

pub const SOURCE: &str = "local";
pub const EXTERNAL_TYPE_ENTRY: &str = "local:entry";
/// Optional per-namespace settings file (see `NamespaceConfig`).
pub const NAMESPACE_FILE: &str = "namespace.yaml";

pub struct Provider {
    base_dir: std::path::PathBuf,
}

impl Provider {
    pub fn new(base_dir: std::path::PathBuf) -> Self {
        Self { base_dir }
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver {
            base_dir: &self.base_dir,
        }
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

/// Parse a `local://` URL into a filesystem path, without resolving
/// namespaces (see `Resolver`).
///
/// - `local:///absolute/path.yaml` → `/absolute/path.yaml`
/// - `local://relative/path.yaml`  → `relative/path.yaml`
pub fn parse_local_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("local://")?;
    Some(PathBuf::from(rest))
}

pub fn to_local_url(path: &Path) -> String {
    format!("local://{}", path.display())
}

/// Each entry kind's name in a namespaced URL and its directory in a
/// namespace.
const KIND_DIRS: [(&str, &str); 4] = [
    ("artist", "artists"),
    ("release_group", "release_groups"),
    ("release", "releases"),
    ("track", "tracks"),
];

/// Where local entries live and what they are called.
///
/// Every directory directly under `base_dir` (symlinks followed) is a
/// *namespace*: `local://<ns>/<kind>/<id>` names the file
/// `<base_dir>/<ns>/<kind>s/<id>.yaml`, e.g. `local://ctbgd/track/kitto` is
/// `ctbgd/tracks/kitto.yaml`. A file in a namespace is always identified by
/// that URL, however it was reached, so an entry keeps its identity when
/// the files move. Any other file is identified by its absolute path
/// (`local:///abs/path.yaml`); relative paths resolve against `base_dir`.
struct Resolver<'a> {
    base_dir: &'a Path,
}

impl Resolver<'_> {
    /// The file `url` names. Relative paths resolve against `relative_to`.
    fn file_of(&self, url: &str, relative_to: &Path) -> Option<PathBuf> {
        let rest = url.strip_prefix("local://").unwrap_or(url);
        if rest.starts_with('/') {
            return Some(PathBuf::from(rest));
        }
        if let Some(file) = self.namespaced_file(rest) {
            return Some(file);
        }
        Some(relative_to.join(rest))
    }

    /// `<ns>/<kind>/<id>` as a file, when that file exists.
    fn namespaced_file(&self, rest: &str) -> Option<PathBuf> {
        let mut parts = rest.splitn(3, '/');
        let (ns, kind, id) = (parts.next()?, parts.next()?, parts.next()?);
        if ns.is_empty() || id.is_empty() || id.contains('/') {
            return None;
        }
        let (_, dir) = KIND_DIRS.iter().find(|(name, _)| *name == kind)?;
        let file = self.base_dir.join(ns).join(dir).join(format!("{id}.yaml"));
        file.is_file().then_some(file)
    }

    /// The URL identifying `file`: namespaced when it sits in a namespace,
    /// else its absolute path.
    fn url_of(&self, file: &Path) -> String {
        let abs = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        self.namespaced_url(&abs)
            .unwrap_or_else(|| to_local_url(&abs))
    }

    fn namespaced_url(&self, abs: &Path) -> Option<String> {
        for ns in std::fs::read_dir(self.base_dir).ok()?.flatten() {
            let name = ns.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(root) = std::fs::canonicalize(ns.path()) else {
                continue;
            };
            if !root.is_dir() {
                continue;
            }
            let Ok(rel) = abs.strip_prefix(&root) else {
                continue;
            };
            let mut parts = rel.iter().map(|p| p.to_string_lossy());
            let (Some(dir), Some(file), None) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let Some((kind, _)) = KIND_DIRS.iter().find(|(_, d)| *d == dir) else {
                continue;
            };
            if let Some(id) = file.strip_suffix(".yaml") {
                return Some(format!("local://{name}/{kind}/{id}"));
            }
        }
        None
    }
}

fn read_entry(path: &std::path::Path) -> Result<LocalEntry, Error> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Error::InvalidUrl(format!("cannot read {}: {e}", path.display())))?;
    serde_yaml_ng::from_str(&content)
        .map_err(|e| Error::InvalidUrl(format!("cannot parse {}: {e}", path.display())))
}

fn read_namespace_config(path: &std::path::Path) -> Result<NamespaceConfig, Error> {
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

/// A child's source as an imported pair. Local files (paths and `local://`
/// URLs, relative ones from the base file's directory) take the URL that
/// identifies them (`Resolver::url_of`), so a file reached from several
/// entries is one pair; a provider URL takes its canonical pair, so it
/// meets that provider's own entry. Anything else stays as written.
fn child_pair(key: &str, value: &str, base: &Path, resolver: &Resolver<'_>) -> (String, String) {
    if (value.starts_with("local://") || !value.contains("://"))
        && let Some(file) = resolver.file_of(value, base)
    {
        return (SOURCE.to_string(), resolver.url_of(&file));
    }
    match canonicalize(value) {
        Some(c) => (c.canonical_source_key.into_owned(), c.canonical_identifier),
        None => (key.to_string(), value.to_string()),
    }
}

fn build_child_ref(child: &LocalChild, base: &Path, resolver: &Resolver<'_>) -> ChildRef {
    let mut sources = ExternalSources::default();
    for (k, vs) in &child.sources {
        for v in vs {
            let (key, id) = child_pair(k, v, base, resolver);
            sources.0.entry(key.into()).or_default().insert(id);
        }
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
        synthetic: false,
    });

    ChildRef {
        image: None,
        entry_type: child.entry_type,
        external_type: EXTERNAL_TYPE_ENTRY.into(),
        sources,
        name: child.name.clone(),
        duration_ms: None,
        appears_on: false,
        position,
        contributions,
        original_relation_kind: child.relation.clone().map(Into::into),
    }
}

fn entry_to_entity_result(
    entry: LocalEntry,
    base: &Path,
    resolver: &Resolver<'_>,
) -> EntityResult<()> {
    let sources = build_sources(&entry.sources);

    let specific_data = match entry.entry_type {
        crate::providers::types::EntryType::Track => EntrySpecificData::Track {
            duration_ms: entry.duration_ms.into_iter().collect(),
            positions: Default::default(),
            songs: vec![],
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
        .map(|c| build_child_ref(c, base, resolver))
        .collect::<Vec<_>>();

    EntityResult {
        privacy: None,
        image: None,
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
    async fn canonicalize(&self, source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
        if source_key != StandardProviderKeys::UNKNOWN_URL && source_key != SOURCE {
            return None;
        }
        if !identifier.starts_with("local://") {
            return None;
        }
        let resolver = self.resolver();
        let path = resolver.file_of(identifier, &self.base_dir)?;
        let entry = read_entry(&path).ok()?;
        Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: resolver.url_of(&path),
            entry_type: entry.entry_type,
            external_type: EXTERNAL_TYPE_ENTRY.into(),
        })
    }
}

#[async_trait]
impl FetchProvider for Provider {
    async fn fetch_entry(
        self: Arc<Self>,
        source_key: &str,
        identifier: &str,
        pool: Arc<EntryFetchOptionsPool>,
        root_id: OptionsId,
    ) -> Result<EntityResult, Error> {
        if source_key != SOURCE {
            return Err(Error::UnsupportedSourceKey(source_key.to_string()));
        }
        let resolver = self.resolver();
        let path = resolver
            .file_of(identifier, &self.base_dir)
            .ok_or_else(|| Error::InvalidUrl(identifier.to_string()))?;
        let entry = read_entry(&path)?;
        let base = path.parent().unwrap_or(Path::new("."));
        let result = entry_to_entity_result(entry, base, &resolver);

        let children = result
            .children
            .iter()
            .map(|s| {
                Ok(Arc::new(filter_children(
                    s.clone(),
                    pool.clone(),
                    root_id,
                    self.clone(),
                    result.specific_data.entry_type(),
                )?))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(EntityResult {
            privacy: None,
            image: result.image,
            children,
            release_date: result.release_date,
            sources: result.sources,
            extra: result.extra,
            specific_data: result.specific_data,
            aliases: result.aliases,
        })
    }

    /// Namespaces whose `namespace.yaml` sets `soft_match: false`. An
    /// unreadable file also excludes its namespace, so a typo can't let
    /// soft-dedup link entries the namespace meant to keep authored.
    fn soft_match_exclusions(&self) -> Vec<(String, String)> {
        let Ok(dirs) = std::fs::read_dir(&self.base_dir) else {
            return Vec::new();
        };
        let mut exclusions = Vec::new();
        for ns in dirs.flatten() {
            let name = ns.file_name().to_string_lossy().into_owned();
            let file = ns.path().join(NAMESPACE_FILE);
            if name.starts_with('.') || !file.is_file() {
                continue;
            }
            let soft_match = read_namespace_config(&file)
                .map(|config| config.soft_match)
                .unwrap_or_else(|e| {
                    tracing::warn!("{e}; excluding namespace {name} from soft-dedup");
                    false
                });
            if !soft_match {
                exclusions.push((SOURCE.to_string(), format!("local://{name}/")));
            }
        }
        exclusions.sort();
        exclusions
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::providers::types::{ChildSource, EntrySpecificData, EntryType, child_next};

    use crate::providers::{CanonicalizeProvider, FetchProvider};

    use super::{
        NAMESPACE_FILE, Provider, SOURCE, entry_to_entity_result, read_entry, to_local_url,
    };

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/providers/backends/local/fixtures")
            .join(name)
    }

    fn fetch(name: &str) -> crate::providers::types::EntityResult<()> {
        let path = fixture_path(name);
        let entry = read_entry(&path).expect("read_entry failed");
        let base = path.parent().unwrap();
        let provider = fixtures_provider();
        entry_to_entity_result(entry, base, &provider.resolver())
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
                ref duration_ms,
                ..
            } if duration_ms == &vec![240000_i64]
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
    async fn test_local_child_original_and_normalized_paths() -> anyhow::Result<()> {
        let parody = fetch("parody.yaml");
        let mut children = parody.children[0].cursor();

        // `../fixtures/artist.yaml` is the same file as `./artist.yaml`.
        let (artist, _) = child_next(&mut children).await?.expect("expected artist");
        let artist_ids = artist.sources.get("local").unwrap();
        let expected = to_local_url(&std::fs::canonicalize(fixture_path("artist.yaml"))?);
        assert_eq!(artist_ids.iter().next(), Some(&expected));

        // A provider URL takes its canonical pair, and the relation carries over.
        let (original, _) = child_next(&mut children).await?.expect("expected original");
        assert_eq!(original.original_relation_kind.as_deref(), Some("cover"));
        let canonical =
            crate::providers::registry::canonicalize("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
                .expect("youtube URLs canonicalize");
        let ids = original
            .sources
            .get(canonical.canonical_source_key.as_ref())
            .expect("keyed by the canonical source");
        assert!(ids.contains(&canonical.canonical_identifier));
        assert!(original.sources.get("url").is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_namespace_config_excludes_soft_match() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        for (ns, config) in [
            ("authored", Some("soft_match: false\n")),
            ("default", None),
            ("matched", Some("soft_match: true\n")),
            ("typo", Some("soft_matc: false\n")),
        ] {
            let dir = base.path().join(ns).join("tracks");
            std::fs::create_dir_all(&dir)?;
            if let Some(config) = config {
                std::fs::write(base.path().join(ns).join(NAMESPACE_FILE), config)?;
            }
        }
        let provider = Provider::new(base.path().to_path_buf());
        // An unparseable file excludes its namespace rather than matching it.
        assert_eq!(
            provider.soft_match_exclusions(),
            [
                (SOURCE.to_string(), "local://authored/".to_string()),
                (SOURCE.to_string(), "local://typo/".to_string()),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_namespaced_urls_identify_files_in_a_namespace() -> anyhow::Result<()> {
        let provider = fixtures_provider();
        let song = fixture_path("demo/tracks/song.yaml");
        let absolute = to_local_url(&std::fs::canonicalize(&song)?);
        for query in [
            "local://demo/track/song",
            absolute.as_str(),
            "local://demo/tracks/song.yaml",
        ] {
            let c = provider
                .canonicalize(super::SOURCE, query)
                .await
                .unwrap_or_else(|| panic!("{query} should canonicalize"));
            assert_eq!(c.canonical_identifier, "local://demo/track/song", "{query}");
            assert_eq!(c.entry_type, EntryType::Track);
        }
        // Outside a namespace, a file keeps its absolute path.
        let outside = provider
            .canonicalize(super::SOURCE, "local://track.yaml")
            .await
            .expect("relative to base_dir");
        assert!(outside.canonical_identifier.starts_with("local:///"));
        assert!(
            provider
                .canonicalize(super::SOURCE, "local://demo/track/nope")
                .await
                .is_none()
        );
        assert!(
            provider
                .canonicalize(super::SOURCE, "https://example.com")
                .await
                .is_none()
        );

        // Children resolve to the same identifiers, by URL or relative path.
        let album = fetch("demo/releases/album.yaml");
        let mut children = album.children[0].cursor();
        let mut ids = Vec::new();
        while let Some((child, _)) = child_next(&mut children).await? {
            ids.extend(child.sources.get("local").unwrap().iter().cloned());
        }
        ids.sort();
        assert_eq!(
            ids,
            ["local://demo/artist/singer", "local://demo/track/song"]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_local_url_resolution() -> anyhow::Result<()> {
        let base = fixture_path("release.yaml");
        let base_dir = base.parent().unwrap();
        let entry = read_entry(&base).expect("read_entry failed");
        let provider = fixtures_provider();
        let result = entry_to_entity_result(entry, base_dir, &provider.resolver());

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

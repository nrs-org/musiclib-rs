//! Discover namespace YAML entries and refresh local entries on startup and
//! when their YAML changes. Polling resolves each identifier afresh, so
//! namespace symlinks and atomic saves
//! work without keeping filesystem watches on obsolete inodes.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use musiclib_rs::{
    musicdb::MusicDb,
    pipeline::{ingest::ingest_entries, state::ImportProgress},
    providers::{
        CanonicalizeProvider, FetchProvider,
        backends::local::{Provider, SOURCE},
        types::{
            ChildMatcher, ChildMatcherExpr, ChildRule, EntryFetchOptions, EntryFetchOptionsPool,
            OptionsId,
        },
    },
};
use sha2::{Digest, Sha256};

type FileHash = [u8; 32];

#[derive(Default)]
struct FileState {
    applied: Option<FileHash>,
    observed: Option<FileHash>,
    rejected: Option<FileHash>,
    unreadable: bool,
}

#[derive(Default)]
struct ReloadProgress(Mutex<HashSet<String>>);

impl ImportProgress for ReloadProgress {
    fn fetched(&self, pair: &(String, String)) {
        if pair.0 == SOURCE {
            self.0.lock().unwrap().insert(pair.1.clone());
        }
    }
}

struct Reloader {
    provider: Arc<Provider>,
    providers: Arc<Vec<Arc<dyn FetchProvider>>>,
    pool: Arc<EntryFetchOptionsPool>,
    root: OptionsId,
    files: HashMap<String, FileState>,
}

impl Reloader {
    fn new(base_dir: PathBuf) -> Self {
        let provider = Arc::new(Provider::new(base_dir));
        // Follow the authored graph to pick up newly added children, without
        // fetching remote discographies or running online soft-dedup on save.
        let mut pool = EntryFetchOptionsPool::default();
        let root = pool.insert(EntryFetchOptions::default());
        pool.patch(
            root,
            EntryFetchOptions {
                child_rules: vec![ChildRule {
                    matcher: ChildMatcherExpr::Matcher(ChildMatcher::Always),
                    options_id: Some(root),
                }],
            },
        );
        Self {
            providers: Arc::new(vec![provider.clone()]),
            provider,
            pool: Arc::new(pool),
            root,
            files: HashMap::new(),
        }
    }

    async fn poll(&mut self, db: &MusicDb) -> anyhow::Result<Vec<String>> {
        let identifiers: BTreeSet<_> = db
            .fetched_identifiers_for_source(SOURCE)
            .await?
            .into_iter()
            .chain(self.provider.namespace_identifiers())
            .collect();
        self.files.retain(|id, _| identifiers.contains(id));
        let mut candidates = Vec::new();
        for identifier in identifiers {
            let Some(path) = self.provider.entry_file(&identifier) else {
                continue;
            };
            let content = match tokio::fs::read(&path).await {
                Ok(content) => content,
                Err(e) => {
                    let file = self.files.entry(identifier.clone()).or_default();
                    file.observed = None;
                    if !file.unreadable {
                        tracing::warn!(
                            "Local reload: cannot read {}: {e}; keeping imported metadata",
                            path.display()
                        );
                        file.unreadable = true;
                    }
                    continue;
                }
            };
            let hash: FileHash = Sha256::digest(&content).into();
            // A first observation is pending, not applied: the saved database
            // may predate this process, and a new file has no database row yet.
            let file = self.files.entry(identifier.clone()).or_default();
            file.unreadable = false;
            let stable = file.observed == Some(hash);
            file.observed = Some(hash);
            if !stable || file.applied == Some(hash) || file.rejected == Some(hash) {
                continue;
            }
            // Validate before starting a run: a half-written or invalid YAML
            // must never replace the last successfully imported metadata.
            let Some(canonical) = self.provider.canonicalize(SOURCE, &identifier).await else {
                tracing::warn!(
                    "Local reload: invalid entry {}; keeping imported metadata",
                    path.display()
                );
                file.rejected = Some(hash);
                continue;
            };
            candidates.push((identifier, canonical.canonical_identifier, hash));
        }
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // Generated namespaces often rewrite many files together. A single
        // run shares traversal claims, fetching each reachable local entry once.
        let progress = Arc::new(ReloadProgress::default());
        ingest_entries(
            db,
            &self.providers,
            &self.pool,
            self.root,
            None,
            candidates.iter().map(|(id, _, _)| id.clone()).collect(),
            Some(progress.clone()),
            None,
            false,
        )
        .await?;
        let fetched = progress.0.lock().unwrap();
        let mut reloaded = Vec::new();
        for (identifier, canonical, hash) in candidates {
            // An ingest can finish even if an individual fetch failed. Only
            // acknowledge roots whose metadata was actually fetched.
            if fetched.contains(&canonical) {
                let file = self.files.get_mut(&identifier).unwrap();
                file.applied = Some(hash);
                file.rejected = None;
                tracing::info!("Reloaded local YAML entry {identifier}");
                reloaded.push(identifier);
            }
        }
        Ok(reloaded)
    }
}

pub async fn run(db: MusicDb, base_dir: PathBuf) {
    tracing::info!(
        "Watching namespace YAML entries under {} and imported absolute local paths",
        base_dir.display()
    );
    let mut reloader = Reloader::new(base_dir);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(e) = reloader.poll(&db).await {
            tracing::warn!("Local YAML reload failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use musiclib_rs::pipeline::ingest::ingest_entry;
    use musiclib_rs::providers::backends::local::to_local_url;
    use std::path::Path;

    async fn import(reloader: &Reloader, db: &MusicDb, url: &str) -> anyhow::Result<i64> {
        Ok(ingest_entry(
            db,
            &reloader.providers,
            &reloader.pool,
            reloader.root,
            None,
            url.to_owned(),
            None,
        )
        .await?
        .entry_id
        .expect("imported local entry"))
    }

    fn write_track(path: &Path, name: &str) -> anyhow::Result<()> {
        std::fs::write(
            path,
            format!("type: track\naliases:\n  - name: {name}\n    primary: true\n"),
        )?;
        Ok(())
    }

    async fn names(db: &MusicDb, id: i64) -> anyhow::Result<Vec<String>> {
        Ok(db
            .alias_rows_for_pairs(&db.pairs_by_entry_ids(&[id]).await?)
            .await?
            .into_iter()
            .map(|a| a.name)
            .collect())
    }

    #[tokio::test]
    async fn local_reload_debounces_and_recovers_from_invalid_or_missing_files()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("song.yaml");
        write_track(&path, "Original")?;
        let url = to_local_url(&path);
        let db = MusicDb::new("sqlite::memory:").await?;
        let mut reloader = Reloader::new(dir.path().to_owned());
        let id = import(&reloader, &db, &url).await?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(reloader.poll(&db).await?, [url.clone()]);

        write_track(&path, "Intermediate")?;
        assert!(reloader.poll(&db).await?.is_empty());
        write_track(&path, "Updated")?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(names(&db, id).await?, ["Original"]);
        assert_eq!(reloader.poll(&db).await?, [url.clone()]);
        assert_eq!(names(&db, id).await?, ["Updated"]);
        assert!(reloader.poll(&db).await?.is_empty());

        std::fs::write(&path, "aliases: [")?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(names(&db, id).await?, ["Updated"]);
        std::fs::remove_file(&path)?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(names(&db, id).await?, ["Updated"]);

        let replacement = dir.path().join("song.tmp");
        write_track(&replacement, "Restored")?;
        std::fs::rename(&replacement, &path)?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(reloader.poll(&db).await?, [url]);
        assert_eq!(names(&db, id).await?, ["Restored"]);
        Ok(())
    }

    #[tokio::test]
    async fn local_reload_tracks_new_imports_and_local_children() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = MusicDb::new("sqlite::memory:").await?;
        let mut reloader = Reloader::new(dir.path().to_owned());
        assert!(reloader.poll(&db).await?.is_empty());
        let tracks = dir.path().join("demo/tracks");
        std::fs::create_dir_all(&tracks)?;
        let path = tracks.join("song.yaml");
        write_track(&path, "Original")?;
        let url = "local://demo/track/song";
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(reloader.poll(&db).await?, [url]);
        let id = db.find_entry_id_by_pair(SOURCE, url).await?.unwrap();

        let artists = dir.path().join("demo/artists");
        std::fs::create_dir_all(&artists)?;
        std::fs::write(
            artists.join("singer.yaml"),
            "type: artist\naliases:\n  - name: Singer\n",
        )?;
        std::fs::write(
            &path,
            "type: track\naliases:\n  - name: Updated\nchildren:\n  - type: artist\n    sources:\n      local: [local://demo/artist/singer]\n    contributions:\n      - role: performer\n        main_artist: true\n",
        )?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(
            reloader.poll(&db).await?,
            ["local://demo/artist/singer", url]
        );
        assert_eq!(names(&db, id).await?, ["Updated"]);
        assert!(
            db.find_entry_id_by_pair(SOURCE, "local://demo/artist/singer")
                .await?
                .is_some()
        );
        assert_eq!(db.main_artist_credits(id).await?.len(), 1);

        write_track(&path, "No credit")?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(reloader.poll(&db).await?, [url]);
        assert!(db.main_artist_credits(id).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn local_reload_refreshes_startup_edits_and_discovers_unreferenced_releases()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for kind in ["tracks", "releases", "release_groups"] {
            std::fs::create_dir_all(dir.path().join("demo").join(kind))?;
        }
        let path = dir.path().join("demo/tracks/song.yaml");
        let url = "local://demo/track/song";
        write_track(&path, "Original")?;
        let db = MusicDb::new("sqlite::memory:").await?;
        let importer = Reloader::new(dir.path().to_owned());
        let id = import(&importer, &db, url).await?;

        // These changes predate the first poll, and neither release is
        // referenced by any imported entry (the ctbgd Respawn regression).
        write_track(&path, "Edited while stopped")?;
        std::fs::write(
            dir.path().join("demo/releases/new.yaml"),
            "type: release\naliases:\n  - name: New release\n",
        )?;
        std::fs::write(
            dir.path().join("demo/release_groups/new.yaml"),
            "type: release_group\naliases:\n  - name: New release group\n",
        )?;
        std::fs::write(
            dir.path().join("demo/namespace.yaml"),
            "soft_match: false\n",
        )?;
        std::fs::write(dir.path().join("demo/tracks/.pending.yaml"), "invalid")?;
        std::fs::write(dir.path().join("demo/tracks/new.tmp"), "invalid")?;

        let mut reloader = Reloader::new(dir.path().to_owned());
        assert!(reloader.poll(&db).await?.is_empty());
        let reloaded = reloader.poll(&db).await?;
        assert_eq!(reloaded.len(), 3);
        assert!(reloaded.iter().any(|id| id == url));
        assert_eq!(names(&db, id).await?, ["Edited while stopped"]);
        for url in ["local://demo/release/new", "local://demo/release_group/new"] {
            assert!(db.find_entry_id_by_pair(SOURCE, url).await?.is_some());
        }
        assert!(reloader.poll(&db).await?.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_reload_follows_namespace_symlinks_and_atomic_saves() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let namespace = tempfile::tempdir()?;
        std::fs::create_dir(namespace.path().join("tracks"))?;
        std::os::unix::fs::symlink(namespace.path(), dir.path().join("demo"))?;
        let path = namespace.path().join("tracks/song.yaml");
        write_track(&path, "Original")?;
        let url = "local://demo/track/song";
        let db = MusicDb::new("sqlite::memory:").await?;
        let mut reloader = Reloader::new(dir.path().to_owned());
        let id = import(&reloader, &db, url).await?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(reloader.poll(&db).await?, [url]);
        let replacement = namespace.path().join("replacement.yaml");
        write_track(&replacement, "Replaced")?;
        std::fs::rename(replacement, path)?;
        assert!(reloader.poll(&db).await?.is_empty());
        assert_eq!(reloader.poll(&db).await?, [url]);
        assert_eq!(names(&db, id).await?, ["Replaced"]);
        Ok(())
    }
}

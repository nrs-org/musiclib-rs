//! Fetch-options file/entry-point picker for the player UI's import form
//! (`player.rs`'s `/api/fetch-options/*` routes and `/api/ingest`'s
//! `fetch_options` field).
//!
//! There is no bundled or hardcoded config here — `<config_dir>/fetch_options/`
//! is a plain directory the user manages themselves (copying in whatever
//! `.yaml` files they want, editing them, removing them), and every call
//! below re-reads it from disk. Dropping in a new file — or a new named
//! entry point inside an existing one — takes effect on the next request,
//! no server restart needed. These are small YAML files parsed by hand-rolled
//! `serde_yaml_ng`, not something that needs a network round trip or heavy
//! compute, so re-reading per request is cheap enough that caching would
//! only buy staleness bugs for no real benefit.
//!
//! A `.yaml` file can define more than one usable starting point (the
//! human-facing format is "a flat map of named option sets where `main` is
//! the root and other keys are reusable" — see
//! `providers::fetch_options_yaml`'s module doc), so picking a preset here is
//! two steps: pick a *file*, then pick which of its top-level keys to start
//! from (skipped client-side when a file only defines `main`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use musiclib_rs::providers::{
    fetch_options_yaml::{YamlFetchDocument, load_from_file_with_root},
    types::{
        ChildMatcher, ChildMatcherExpr, ChildRule, EntryFetchOptions, EntryFetchOptionsPool,
        OptionsId,
    },
};

/// Reserved id for the built-in fallback (see `shallow`) — not a real file,
/// so it can't collide with a genuine `<file-stem>.yaml`.
pub const SHALLOW_ID: &str = "__shallow__";

/// The conventional root key in a fetch-options file — what a file's picked
/// entry point defaults to when the caller doesn't say otherwise (e.g. a
/// request that names a file but omits `entry_point`).
pub const DEFAULT_ENTRY_POINT: &str = "main";

/// The built-in "1 level deep" fallback: fetch every direct child of the
/// ingested entry and stop. Used both as the process-wide default when no
/// `--fetch-options` is given (`main.rs`), and as the always-available
/// `__shallow__` choice here — the only option that isn't backed by a file,
/// for when `<config_dir>/fetch_options/` is empty or the user just wants a
/// quick default without picking a config.
pub fn shallow() -> (Arc<EntryFetchOptionsPool>, OptionsId) {
    let mut pool = EntryFetchOptionsPool::default();
    let root_id = pool.insert(EntryFetchOptions {
        child_rules: vec![ChildRule {
            matcher: ChildMatcherExpr::Matcher(ChildMatcher::Always),
            options_id: Some(EntryFetchOptionsPool::DEFAULT_ID),
        }],
    });
    (Arc::new(pool), root_id)
}

pub struct FileEntry {
    /// The bare filename stem (e.g. `fetch_discography` for
    /// `fetch_discography.yaml`) — what the client sends back as `file` in
    /// `/api/ingest`'s `fetch_options` field.
    pub id: String,
}

/// Validates `id` is safe to turn into `dir.join(format!("{id}.yaml"))`:
/// non-empty and restricted to letters/digits/underscore/hyphen, so it can
/// never contain a path separator or `..`. Every function below that turns a
/// client-supplied id into a path calls this first.
fn valid_file_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn yaml_paths_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    paths.sort();
    paths
}

/// Every `.yaml` file directly under `dir` (not recursive — a file's own
/// cross-file `::` references still resolve normally when it's actually
/// loaded, only top-level discovery here is non-recursive). Re-scans `dir`
/// on every call — see the module doc for why. Does not validate that each
/// file parses; that's `entry_points`'/`resolve`'s job once a specific file
/// is chosen, so one broken file doesn't hide every other file from the
/// picker.
pub fn list_files(dir: &Path) -> Vec<FileEntry> {
    yaml_paths_in(dir)
        .into_iter()
        .filter_map(|path| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|stem| FileEntry {
                    id: stem.to_string(),
                })
        })
        .collect()
}

/// The entry-point names `file_id` (as returned by `list_files`) offers —
/// its YAML document's top-level keys, `main` first if present, the rest
/// alphabetical. Only parses `file_id` itself, not anything it cross-file
/// references, so this stays fast and correct even if a referenced file is
/// broken. `Err` carries a message fit to show the user directly (invalid
/// id, missing file, or a YAML parse error).
pub async fn entry_points(dir: &Path, file_id: &str) -> Result<Vec<String>, String> {
    if !valid_file_id(file_id) {
        return Err("invalid file id".to_string());
    }
    let path = dir.join(format!("{file_id}.yaml"));
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    let doc: YamlFetchDocument =
        serde_yaml_ng::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let mut names: Vec<String> = doc.0.into_keys().collect();
    names.sort();
    if let Some(i) = names.iter().position(|n| n == DEFAULT_ENTRY_POINT) {
        let main = names.remove(i);
        names.insert(0, main);
    }
    Ok(names)
}

/// Resolves one `(file_id, entry_point)` pair to a loaded pool. `file_id ==
/// SHALLOW_ID` returns the built-in fallback regardless of `entry_point`.
/// Otherwise loads only the one file named via
/// `load_from_file_with_root(path, entry_point)` — unlike `list_files`, this
/// never scans the directory, so it stays cheap and correct even with many
/// files present. Returns `None` for an invalid id, a file that doesn't
/// exist, or one that fails to parse (logged as a warning either way, since
/// a `None` here becomes a generic 400 to the client).
pub async fn resolve(
    dir: &Path,
    file_id: &str,
    entry_point: &str,
) -> Option<(Arc<EntryFetchOptionsPool>, OptionsId)> {
    if file_id == SHALLOW_ID {
        return Some(shallow());
    }
    if !valid_file_id(file_id) {
        return None;
    }
    let path = dir.join(format!("{file_id}.yaml"));
    if !path.is_file() {
        return None;
    }
    match load_from_file_with_root(&path, entry_point).await {
        Ok((pool, root_id, _hash)) => Some((pool, root_id)),
        Err(e) => {
            tracing::warn!("Failed to load fetch-options file {}: {e}", path.display());
            None
        }
    }
}

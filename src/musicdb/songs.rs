//! Song editing: operations on a version of a song as a whole (detach it,
//! make it the original, set what it was made from) instead of on single
//! relation rows. A song is an edition group (`EditionProjection`), and its
//! membership depends only on which entries some path of edges connects,
//! so removing one row rarely changes anything; these operations write
//! whatever rows the intent needs.
//!
//! Each edit runs in one transaction and returns the rows it changed as
//! they were before ([`RowImage`]). [`MusicDb::restore_relation_rows`]
//! writes images back and returns the rows it replaced, so the same call
//! serves undo and redo.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter,
    TransactionTrait, sea_query,
};
use serde::{Deserialize, Serialize};

use super::{
    DERIVED_FROM, DIFFERENT_IDENTITY, DIFFERENT_SONG, EDITION_KINDS, EditionProjection, Error,
    MusicDb, PROVIDER_ORIGIN, SAME_IDENTITY, SoftIdentityProjection, entry_relation,
    extra_derived_entry, ordered_pair, write_derivation,
};

/// A relation row's state, without its key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowState {
    pub confidence: f64,
    pub origin: String,
    pub enabled: bool,
    pub extra: Option<String>,
}

/// A relation row as it was: `row` is `None` when it didn't exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowImage {
    pub entry_a: i64,
    pub entry_b: i64,
    pub kind: String,
    pub row: Option<RowState>,
}

/// The key of one relation row, for [`SongEdit::Disable`].
#[derive(Debug, Clone, Deserialize)]
pub struct RowKey {
    pub a: i64,
    pub b: i64,
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SongEdit {
    /// Take `entries` out of their song for good: their edition rows to the
    /// rest of it are tombstoned and a `different_song` row keeps them out.
    /// Rows among `entries` themselves stay, so they leave together.
    Detach { entries: Vec<i64> },
    /// Make `entry` its song's original: every version that hung off the
    /// old original (and the old original itself) now derives from it.
    SetOriginal { entry: i64 },
    /// Record that each of `entries` was made from `parent` by
    /// `transformation` ("cover", "instrumental", ...). Inside one song this
    /// replaces what the entry was made from; across songs it merges the
    /// two. `unblock` lifts a `different_song` block between them.
    SetParent {
        entries: Vec<i64>,
        parent: i64,
        #[serde(default)]
        transformation: Option<String>,
        #[serde(default)]
        unblock: bool,
    },
    /// Detach `entries` from their song and put them under `parent` in
    /// another song.
    Move {
        entries: Vec<i64>,
        parent: i64,
        #[serde(default)]
        transformation: Option<String>,
    },
    /// Record that `derived` was made from `original`, replacing whatever
    /// the pair's own rows said (`MusicDb::set_edition_derivation`).
    Derive {
        original: i64,
        derived: i64,
        #[serde(default)]
        transformation: Option<String>,
    },
    /// Assert a manual `kind` row between `a` and `b`. The caller vets
    /// `kind` (the server's `RELATE_KINDS`).
    Relate { a: i64, b: i64, kind: String },
    /// Tombstone these rows (not identity rows: those belong to
    /// `record_identity_feedback`).
    Disable { rows: Vec<RowKey> },
    /// Re-enable tombstoned rows, keeping their origin and metadata (not
    /// identity rows).
    Enable { rows: Vec<RowKey> },
}

impl SongEdit {
    fn seeds(&self) -> Vec<i64> {
        match self {
            Self::Detach { entries } => entries.clone(),
            Self::SetOriginal { entry } => vec![*entry],
            Self::SetParent {
                entries, parent, ..
            }
            | Self::Move {
                entries, parent, ..
            } => entries.iter().copied().chain([*parent]).collect(),
            Self::Derive {
                original, derived, ..
            } => vec![*original, *derived],
            Self::Relate { a, b, .. } => vec![*a, *b],
            Self::Disable { rows } | Self::Enable { rows } => {
                rows.iter().flat_map(|r| [r.a, r.b]).collect()
            }
        }
    }
}

/// The projections an edit reads, with the lookups it needs.
struct Songs {
    identity: SoftIdentityProjection,
    editions: EditionProjection,
}

impl Songs {
    fn canon(&self, id: i64) -> i64 {
        self.identity.component_of(id).unwrap_or(id)
    }

    /// The entries of identity `canonical`.
    fn entries_of(&self, canonical: i64) -> Vec<i64> {
        self.identity
            .members_by_component
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| vec![canonical])
    }

    /// Every entry in `id`'s song.
    fn song_entries(&self, id: i64) -> Vec<i64> {
        self.editions
            .members_of(self.canon(id))
            .into_iter()
            .flat_map(|member| self.entries_of(member))
            .collect()
    }

    fn song_of(&self, id: i64) -> i64 {
        self.editions.group_of(self.canon(id))
    }

    /// `ancestor` is `id` or what it was made from, at any depth.
    fn descends_from(&self, id: i64, ancestor: i64) -> bool {
        let mut current = id;
        let mut seen = HashSet::new();
        loop {
            if current == ancestor {
                return true;
            }
            if !seen.insert(current) {
                return false;
            }
            match self.editions.parent_of(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }
}

impl MusicDb {
    /// Applies `edit` and returns the rows it changed as they were before.
    pub async fn edit_song(&self, edit: SongEdit) -> Result<Vec<RowImage>, Error> {
        let seeds = edit.seeds();
        if seeds.is_empty() {
            return Err(Error::InvalidInput("no entries to edit".into()));
        }
        let (identity, editions) = self.display_projections_around(&seeds).await?;
        let songs = Songs { identity, editions };
        let nodes: Vec<i64> = seeds
            .iter()
            .flat_map(|&id| songs.song_entries(id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let _guard = self.write_lock.lock().await;
        let txn = self.db.begin().await?;
        let before = rows_among(&txn, &nodes).await?;
        match edit {
            SongEdit::Detach { entries } => detach(&txn, &songs, &entries).await?,
            SongEdit::SetOriginal { entry } => set_original(&txn, &songs, entry, &before).await?,
            SongEdit::SetParent {
                entries,
                parent,
                transformation,
                unblock,
            } => {
                set_parent(
                    &txn,
                    &songs,
                    &entries,
                    parent,
                    transformation.as_deref(),
                    unblock,
                    &before,
                )
                .await?
            }
            SongEdit::Move {
                entries,
                parent,
                transformation,
            } => {
                if entries
                    .iter()
                    .any(|&e| songs.song_of(e) == songs.song_of(parent))
                {
                    return Err(Error::InvalidInput(
                        "already in that song: set what it was made from instead".into(),
                    ));
                }
                detach(&txn, &songs, &entries).await?;
                let parent_canon = songs.canon(parent);
                for &entry in &entries {
                    let child = songs.canon(entry);
                    write_derivation(
                        &txn,
                        (parent_canon, &songs.entries_of(parent_canon)),
                        (child, &songs.entries_of(child)),
                        transformation.as_deref(),
                    )
                    .await?;
                }
            }
            SongEdit::Derive {
                original,
                derived,
                transformation,
            } => {
                let (original, derived) = (songs.canon(original), songs.canon(derived));
                if original == derived {
                    return Err(Error::InvalidInput(
                        "both entries are the same identity, not two editions".into(),
                    ));
                }
                write_derivation(
                    &txn,
                    (original, &songs.entries_of(original)),
                    (derived, &songs.entries_of(derived)),
                    transformation.as_deref(),
                )
                .await?;
            }
            SongEdit::Relate { a, b, kind } => {
                if a == b || is_identity_kind(&kind) {
                    return Err(Error::InvalidInput(format!("can't relate as {kind:?}")));
                }
                let row = RowState {
                    confidence: 1.0,
                    origin: "manual".into(),
                    enabled: true,
                    extra: None,
                };
                upsert_row(&txn, ordered_pair(a, b), &kind, &row).await?;
            }
            SongEdit::Disable { rows } => set_enabled(&txn, rows, false).await?,
            SongEdit::Enable { rows } => set_enabled(&txn, rows, true).await?,
        }
        let after = rows_among(&txn, &nodes).await?;
        txn.commit().await?;
        Ok(changed(&before, &after))
    }

    /// Writes `images` back (deleting rows whose image is `None`) and
    /// returns the rows they replaced: undo, and the result is its redo.
    pub async fn restore_relation_rows(
        &self,
        images: Vec<RowImage>,
    ) -> Result<Vec<RowImage>, Error> {
        let _guard = self.write_lock.lock().await;
        let txn = self.db.begin().await?;
        let mut replaced = Vec::with_capacity(images.len());
        for image in images {
            let (a, b) = ordered_pair(image.entry_a, image.entry_b);
            let current = entry_relation::Entity::find_by_id((a, b, image.kind.clone()))
                .one(&txn)
                .await?;
            replaced.push(RowImage {
                entry_a: a,
                entry_b: b,
                kind: image.kind.clone(),
                row: current.map(state_of),
            });
            match image.row {
                None => {
                    entry_relation::Entity::delete_by_id((a, b, image.kind))
                        .exec(&txn)
                        .await?;
                }
                Some(row) => upsert_row(&txn, (a, b), &image.kind, &row).await?,
            }
        }
        txn.commit().await?;
        Ok(replaced)
    }
}

async fn detach<C: ConnectionTrait>(txn: &C, songs: &Songs, entries: &[i64]) -> Result<(), Error> {
    let Some(&first) = entries.first() else {
        return Err(Error::InvalidInput("no entries to detach".into()));
    };
    let song = songs.song_of(first);
    if entries.iter().any(|&e| songs.song_of(e) != song) {
        return Err(Error::InvalidInput(
            "detach versions of one song at a time".into(),
        ));
    }
    let leaving: BTreeSet<i64> = entries.iter().map(|&e| songs.canon(e)).collect();
    let staying: Vec<i64> = songs
        .editions
        .members_of(song)
        .into_iter()
        .filter(|m| !leaving.contains(m))
        .collect();
    let Some(&first_staying) = staying.first() else {
        return Err(Error::InvalidInput(
            "that's the whole song: nothing to detach from".into(),
        ));
    };
    let leaving_entries: Vec<i64> = leaving.iter().flat_map(|&c| songs.entries_of(c)).collect();
    let staying_entries: Vec<i64> = staying.iter().flat_map(|&c| songs.entries_of(c)).collect();
    entry_relation::Entity::update_many()
        .col_expr(entry_relation::Column::Enabled, false.into())
        .filter(between(&leaving_entries, &staying_entries))
        .filter(entry_relation::Column::Kind.is_in(EDITION_KINDS.iter().copied()))
        .exec(txn)
        .await?;
    // One block per detached identity is enough: the projection checks
    // blocks between whole songs, so it also covers versions added later.
    let anchor = songs
        .editions
        .original_of(song)
        .filter(|original| !leaving.contains(original))
        .unwrap_or(first_staying);
    let block = RowState {
        confidence: 1.0,
        origin: "manual".into(),
        enabled: true,
        extra: None,
    };
    for &canonical in &leaving {
        upsert_row(txn, ordered_pair(canonical, anchor), DIFFERENT_SONG, &block).await?;
    }
    Ok(())
}

async fn set_original<C: ConnectionTrait>(
    txn: &C,
    songs: &Songs,
    entry: i64,
    rows: &BTreeMap<(i64, i64, String), RowState>,
) -> Result<(), Error> {
    let new = songs.canon(entry);
    let song = songs.song_of(entry);
    let old = songs.editions.original_of(song);
    if old == Some(new) {
        return Ok(());
    }
    let members = songs.editions.members_of(song);
    // Versions that hung off the old original, and the old original itself.
    let mut targets: BTreeSet<i64> = members
        .iter()
        .copied()
        .filter(|&m| m != new && songs.editions.parent_of(m).is_none())
        .collect();
    // What `new` was made from, up its chain.
    let mut current = new;
    while let Some(parent) = songs.editions.parent_of(current) {
        if !targets.insert(parent) {
            break;
        }
        current = parent;
    }
    // Any row that says `new` was made from another member.
    for ((a, b, kind), row) in rows {
        if !row.enabled || !EDITION_KINDS.contains(&kind.as_str()) {
            continue;
        }
        let derived = row
            .extra
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .and_then(|extra: serde_json::Value| extra_derived_entry(&extra))
            .map(|id| songs.canon(id));
        let (ca, cb) = (songs.canon(*a), songs.canon(*b));
        if derived == Some(new) && (ca == new || cb == new) {
            let other = if ca == new { cb } else { ca };
            if members.contains(&other) {
                targets.insert(other);
            }
        }
    }
    targets.remove(&new);
    for target in targets {
        // Keep what the version is (a cover stays a cover); the old original
        // becomes an unspecified other version.
        let transformation = old
            .filter(|&o| o != target)
            .and_then(|o| songs.editions.edge_label(target, o))
            .and_then(|(kind, transformation)| {
                transformation
                    .map(str::to_string)
                    .or_else(|| (kind != DERIVED_FROM).then(|| kind.to_string()))
            });
        write_derivation(
            txn,
            (new, &songs.entries_of(new)),
            (target, &songs.entries_of(target)),
            transformation.as_deref(),
        )
        .await?;
    }
    Ok(())
}

async fn set_parent<C: ConnectionTrait>(
    txn: &C,
    songs: &Songs,
    entries: &[i64],
    parent: i64,
    transformation: Option<&str>,
    unblock: bool,
    rows: &BTreeMap<(i64, i64, String), RowState>,
) -> Result<(), Error> {
    let parent = songs.canon(parent);
    let mut groups = songs.editions.song_groups();
    for &entry in entries {
        let child = songs.canon(entry);
        if child == parent {
            return Err(Error::InvalidInput(
                "a version can't be made from itself".into(),
            ));
        }
        let same_song = songs.song_of(child) == songs.song_of(parent);
        if same_song && songs.descends_from(parent, child) {
            return Err(Error::InvalidInput(
                "that version was made from this one: move it first".into(),
            ));
        }
        if same_song && songs.editions.original_of(songs.song_of(child)) == Some(child) {
            return Err(Error::InvalidInput(
                "this is the song's original: make another version the original first".into(),
            ));
        }
        if !same_song && let Some(block) = groups.blocker(child, parent) {
            if !unblock {
                return Err(Error::Blocked(format!(
                    "Entries {} and {} are marked as different songs",
                    block.0, block.1
                )));
            }
            entry_relation::Entity::update_many()
                .col_expr(entry_relation::Column::Enabled, false.into())
                .filter(between(
                    &songs.song_entries(child),
                    &songs.song_entries(parent),
                ))
                .filter(entry_relation::Column::Kind.eq(DIFFERENT_SONG))
                .exec(txn)
                .await?;
        }
        if same_song {
            // Replace what the version was made from: tombstone the
            // non-provider rows that name it derived from another member
            // (a person's row outranks a provider's anyway).
            let child_entries = songs.entries_of(child);
            for ((a, b, kind), row) in rows {
                if !row.enabled
                    || row.origin == PROVIDER_ORIGIN
                    || !EDITION_KINDS.contains(&kind.as_str())
                {
                    continue;
                }
                let derived = row
                    .extra
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .and_then(|extra: serde_json::Value| extra_derived_entry(&extra));
                let other = if child_entries.contains(a) { *b } else { *a };
                if derived.is_some_and(|d| child_entries.contains(&d))
                    && songs.canon(other) != parent
                {
                    entry_relation::Entity::update_many()
                        .col_expr(entry_relation::Column::Enabled, false.into())
                        .filter(entry_relation::Column::EntryA.eq(*a))
                        .filter(entry_relation::Column::EntryB.eq(*b))
                        .filter(entry_relation::Column::Kind.eq(kind.as_str()))
                        .exec(txn)
                        .await?;
                }
            }
        }
        write_derivation(
            txn,
            (parent, &songs.entries_of(parent)),
            (child, &songs.entries_of(child)),
            transformation,
        )
        .await?;
        let _ = groups.join(child, parent);
    }
    Ok(())
}

fn is_identity_kind(kind: &str) -> bool {
    [SAME_IDENTITY, DIFFERENT_IDENTITY, "variant"].contains(&kind)
}

async fn set_enabled<C: ConnectionTrait>(
    txn: &C,
    rows: Vec<RowKey>,
    enabled: bool,
) -> Result<(), Error> {
    for key in rows {
        if is_identity_kind(&key.kind) {
            return Err(Error::InvalidInput(
                "identity links are edited through link/unlink".into(),
            ));
        }
        let (a, b) = ordered_pair(key.a, key.b);
        entry_relation::Entity::update_many()
            .col_expr(entry_relation::Column::Enabled, enabled.into())
            .filter(entry_relation::Column::EntryA.eq(a))
            .filter(entry_relation::Column::EntryB.eq(b))
            .filter(entry_relation::Column::Kind.eq(key.kind))
            .exec(txn)
            .await?;
    }
    Ok(())
}

/// Rows with one endpoint in `left` and the other in `right`.
fn between(left: &[i64], right: &[i64]) -> Condition {
    let side = |a: &[i64], b: &[i64]| {
        Condition::all()
            .add(entry_relation::Column::EntryA.is_in(a.iter().copied()))
            .add(entry_relation::Column::EntryB.is_in(b.iter().copied()))
    };
    Condition::any()
        .add(side(left, right))
        .add(side(right, left))
}

async fn rows_among<C: ConnectionTrait>(
    txn: &C,
    nodes: &[i64],
) -> Result<BTreeMap<(i64, i64, String), RowState>, Error> {
    Ok(entry_relation::Entity::find()
        .filter(entry_relation::Column::EntryA.is_in(nodes.iter().copied()))
        .filter(entry_relation::Column::EntryB.is_in(nodes.iter().copied()))
        .all(txn)
        .await?
        .into_iter()
        .map(|row| ((row.entry_a, row.entry_b, row.kind.clone()), state_of(row)))
        .collect())
}

fn state_of(row: entry_relation::Model) -> RowState {
    RowState {
        confidence: row.confidence,
        origin: row.origin,
        enabled: row.enabled,
        extra: row.extra,
    }
}

/// The rows that differ between `before` and `after`, as they were before.
fn changed(
    before: &BTreeMap<(i64, i64, String), RowState>,
    after: &BTreeMap<(i64, i64, String), RowState>,
) -> Vec<RowImage> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|(a, b, kind)| RowImage {
            entry_a: *a,
            entry_b: *b,
            kind: kind.clone(),
            row: before.get(&(*a, *b, kind.clone())).cloned(),
        })
        .collect()
}

async fn upsert_row<C: ConnectionTrait>(
    txn: &C,
    (a, b): (i64, i64),
    kind: &str,
    row: &RowState,
) -> Result<(), Error> {
    entry_relation::Entity::insert(entry_relation::ActiveModel {
        entry_a: Set(a),
        entry_b: Set(b),
        kind: Set(kind.to_string()),
        confidence: Set(row.confidence),
        origin: Set(row.origin.clone()),
        enabled: Set(row.enabled),
        extra: Set(row.extra.clone()),
    })
    .on_conflict(
        sea_query::OnConflict::columns([
            entry_relation::Column::EntryA,
            entry_relation::Column::EntryB,
            entry_relation::Column::Kind,
        ])
        .update_columns([
            entry_relation::Column::Confidence,
            entry_relation::Column::Origin,
            entry_relation::Column::Enabled,
            entry_relation::Column::Extra,
        ])
        .to_owned(),
    )
    .exec(txn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::types::EntryType;

    async fn track(db: &MusicDb) -> i64 {
        db.insert_entry(Some(EntryType::Track)).await.unwrap()
    }

    async fn cover(db: &MusicDb, original: i64, cover: i64) {
        let extra = format!(r#"{{"derived_entry":{cover},"source_entry":{original}}}"#);
        db.upsert_relation(
            original,
            cover,
            "cover",
            0.85,
            PROVIDER_ORIGIN,
            Some(&extra),
        )
        .await
        .unwrap();
    }

    async fn editions(db: &MusicDb) -> EditionProjection {
        db.edition_projection_for_display().await.unwrap()
    }

    /// The original and two covers; the matcher also related the covers.
    async fn song() -> (MusicDb, i64, i64, i64) {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let (original, a, b) = (track(&db).await, track(&db).await, track(&db).await);
        cover(&db, original, a).await;
        cover(&db, original, b).await;
        let extra = format!(r#"{{"derived_entry":{b},"source_entry":{a}}}"#);
        db.upsert_relation(a, b, DERIVED_FROM, 0.7, "heuristic", Some(&extra))
            .await
            .unwrap();
        (db, original, a, b)
    }

    #[tokio::test]
    async fn detach_sticks_and_undoes() {
        let (db, original, a, b) = song().await;
        let undo = db
            .edit_song(SongEdit::Detach { entries: vec![b] })
            .await
            .unwrap();
        let projection = editions(&db).await;
        assert_eq!(projection.members_of(original), vec![original, a]);
        assert_eq!(projection.members_of(b), vec![b]);

        // A provider re-import re-enables its row; the block still holds.
        cover(&db, original, b).await;
        assert_eq!(editions(&db).await.members_of(b), vec![b]);

        let redo = db.restore_relation_rows(undo).await.unwrap();
        assert_eq!(editions(&db).await.members_of(b), vec![original, a, b]);
        db.restore_relation_rows(redo).await.unwrap();
        assert_eq!(editions(&db).await.members_of(b), vec![b]);
    }

    #[tokio::test]
    async fn set_original_repoints_the_song() {
        let (db, original, a, b) = song().await;
        db.edit_song(SongEdit::SetOriginal { entry: a })
            .await
            .unwrap();
        let projection = editions(&db).await;
        assert_eq!(projection.original_of(original), Some(a));
        assert_eq!(projection.parent_of(original), None);
        assert_eq!(projection.parent_of(b), None);
        assert_eq!(
            projection.edge_label(a, b),
            Some((DERIVED_FROM, Some("cover")))
        );
    }

    #[tokio::test]
    async fn set_parent_builds_a_chain_and_respects_blocks() {
        let (db, original, a, b) = song().await;
        db.edit_song(SongEdit::SetParent {
            entries: vec![b],
            parent: a,
            transformation: Some("remix".into()),
            unblock: false,
        })
        .await
        .unwrap();
        assert_eq!(editions(&db).await.parent_of(b), Some(a));

        db.edit_song(SongEdit::Detach { entries: vec![b] })
            .await
            .unwrap();
        let blocked = db
            .edit_song(SongEdit::SetParent {
                entries: vec![b],
                parent: original,
                transformation: None,
                unblock: false,
            })
            .await;
        assert!(matches!(blocked, Err(Error::Blocked(_))), "{blocked:?}");
        db.edit_song(SongEdit::SetParent {
            entries: vec![b],
            parent: original,
            transformation: Some("cover".into()),
            unblock: true,
        })
        .await
        .unwrap();
        assert_eq!(editions(&db).await.members_of(b), vec![original, a, b]);
    }

    #[tokio::test]
    async fn move_takes_a_version_to_another_song() {
        let (db, original, a, b) = song().await;
        let (other, other_cover) = (track(&db).await, track(&db).await);
        cover(&db, other, other_cover).await;
        db.edit_song(SongEdit::Move {
            entries: vec![b],
            parent: other,
            transformation: Some("cover".into()),
        })
        .await
        .unwrap();
        let projection = editions(&db).await;
        assert_eq!(projection.members_of(original), vec![original, a]);
        assert_eq!(projection.members_of(other), vec![b, other, other_cover]);
        assert_eq!(projection.original_of(b), Some(other));
    }
}

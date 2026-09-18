use std::collections::{BTreeMap, HashMap, HashSet};

use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, ConnectionTrait, Database, DatabaseConnection, DbErr,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, Statement, TransactionTrait, sea_query,
};
use tracing::warn;

use crate::providers::types::{Alias, Contribution, EntrySpecificData, EntryType};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Database error: {0}")]
    Database(#[from] DbErr),
    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

// Pair-centric schema. Metadata lives on `entry_source` (keyed by the
// (source, identifier) pair); `entry` is a grouping primitive whose id is
// referenced from `entry_source.entry_id` only. Aliases, child edges, and
// contributions are pair-keyed, so a DB-level entry merge only needs to
// re-point `entry_source` rows.

mod entry {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub entry_type: String,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_source {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_source")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub source: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub identifier: String,
        pub entry_id: i64,
        pub release_date: Option<String>,
        pub fetched_at: i64,
        // Track-specific
        pub duration_ms: Option<i64>,
        /// JSON array of all known durations in milliseconds, sorted and deduped.
        /// Supersedes `duration_ms` when present; `duration_ms` holds only the
        /// first element for backward compatibility.
        pub duration_ms_all: Option<String>,
        // Release-specific
        pub release_type: Option<String>,
        pub num_discs: Option<i32>,
        pub num_tracks: Option<i32>,
        // ReleaseGroup-specific
        pub primary_type: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_alias {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_alias")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub source: String,
        pub identifier: String,
        pub name: String,
        pub locale: Option<String>,
        pub extra: Option<String>,
        pub primary: bool,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_child {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_child")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub parent_source: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub parent_identifier: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub child_source: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub child_identifier: String,
        pub disc_no: Option<i32>,
        pub track_no: Option<i32>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod contribution {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "contribution")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub source: String,
        pub identifier: String,
        pub artist_source: String,
        pub artist_identifier: String,
        pub role: String,
        pub main_artist: bool,
        pub extra: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

// Persisted candidate-retrieval blocking index. One row per (entry, block key)
// the entry participates in — `block_key` already encodes the channel and
// entry-type discriminant (e.g. "exact|track|foo", "dur|482|96"), so a single
// indexed column lookup answers "who else shares this key" without scanning
// the rest of the library. Maintained incrementally: `reindex_block_keys`
// deletes and re-inserts the rows for a given entry_id set, so it stays
// correct as aliases/credits/tracklists change. See `block_keys_for_entry` in
// `pipeline::softmatch` for key derivation.
mod dedup_block_key {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "dedup_block_key")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_id: i64,
        #[sea_orm(primary_key, auto_increment = false, indexed)]
        pub block_key: String,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

// Soft-dedup relation edges. Each row is a heuristic (or manual) assertion
// that two entries are related in some way. `entry_a < entry_b` is enforced
// at insertion time so there is at most one row per (entry_a, entry_b, kind)
// triple regardless of which side was passed first. Disabling a row (enabled=0)
// tombstones the decision without losing history.
mod entry_relation {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_relation")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_a: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_b: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub kind: String,
        pub confidence: f64,
        pub origin: String,
        pub enabled: bool,
        pub extra: Option<String>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// Current model suggestion queue. Re-running the same model refreshes its
/// evidence without resetting review state; a new model version gets its own
/// row so comparisons and rollback remain possible.
mod dedup_suggestion {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "dedup_suggestion")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_a: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_b: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub model_version: String,
        pub probability: f64,
        pub decision: String,
        pub candidate_channels: String,
        pub features: String,
        pub evidence: String,
        pub status: String,
        pub created_at: i64,
        pub updated_at: i64,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// Immutable human/model judgments used to reconstruct corrections and export
/// training data. A correction appends a row pointing at `supersedes_id`.
mod dedup_feedback {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "dedup_feedback")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub entry_a: i64,
        pub entry_b: i64,
        pub judgment: String,
        pub origin: String,
        pub model_version: Option<String>,
        pub probability: Option<f64>,
        pub candidate_channels: Option<String>,
        pub features: Option<String>,
        pub evidence: Option<String>,
        pub note: Option<String>,
        pub supersedes_id: Option<i64>,
        pub created_at: i64,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

// Dedup bookkeeping. `dedup_state` records the last-applied mtime of each barrier
// config file (the cheap "did it change" gate). `entry_dedup` is a many-to-many
// tag table recording which config files currently affect which entries, so when
// a config is changed or removed we can recover the entries it used to touch
// (needed to re-merge after a barrier is relaxed — its anchors are gone from the
// new config, so only the persisted tag knows which entries to re-import).
mod dedup_state {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "dedup_state")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub config_path: String,
        pub mtime_ns: i64,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod entry_dedup {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "entry_dedup")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub entry_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub config_path: String,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub struct MusicDb {
    db: DatabaseConnection,
}

pub const SAME_IDENTITY: &str = "same_identity";
pub const DIFFERENT_IDENTITY: &str = "different_identity";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityJudgment {
    Same,
    Different,
    Unsure,
}

impl IdentityJudgment {
    fn as_str(self) -> &'static str {
        match self {
            Self::Same => SAME_IDENTITY,
            Self::Different => DIFFERENT_IDENTITY,
            Self::Unsure => "unsure",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewDedupFeedback {
    pub entry_a: i64,
    pub entry_b: i64,
    pub judgment: IdentityJudgment,
    pub origin: String,
    pub model_version: Option<String>,
    pub probability: Option<f64>,
    pub candidate_channels: Option<String>,
    pub features: Option<String>,
    pub evidence: Option<String>,
    pub note: Option<String>,
    pub supersedes_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewDedupSuggestion {
    pub entry_a: i64,
    pub entry_b: i64,
    pub model_version: String,
    pub probability: f64,
    pub decision: String,
    pub candidate_channels: String,
    pub features: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DedupSuggestionRow {
    pub entry_a: i64,
    pub entry_b: i64,
    pub model_version: String,
    pub probability: f64,
    pub decision: String,
    pub candidate_channels: String,
    pub features: String,
    pub evidence: String,
    pub status: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DedupFeedbackRow {
    pub id: i64,
    pub entry_a: i64,
    pub entry_b: i64,
    pub judgment: String,
    pub origin: String,
    pub model_version: Option<String>,
    pub probability: Option<f64>,
    pub candidate_channels: Option<String>,
    pub features: Option<String>,
    pub evidence: Option<String>,
    pub note: Option<String>,
    pub supersedes_id: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftIdentityConflict {
    pub same_edge: (i64, i64),
    pub different_edge: (i64, i64),
}

#[derive(Debug, Clone, Default)]
pub struct SoftIdentityProjection {
    pub component_by_entry: HashMap<i64, i64>,
    pub members_by_component: BTreeMap<i64, Vec<i64>>,
    pub conflicts: Vec<SoftIdentityConflict>,
    cannot_components: HashSet<(i64, i64)>,
}

impl SoftIdentityProjection {
    pub fn component_of(&self, entry_id: i64) -> Option<i64> {
        self.component_by_entry.get(&entry_id).copied()
    }

    pub fn are_same(&self, a: i64, b: i64) -> bool {
        a == b
            || matches!((self.component_of(a), self.component_of(b)), (Some(x), Some(y)) if x == y)
    }

    pub fn are_different(&self, a: i64, b: i64) -> bool {
        let Some(a) = self.component_of(a) else {
            return false;
        };
        let Some(b) = self.component_of(b) else {
            return false;
        };
        self.cannot_components.contains(&ordered_pair(a, b))
    }
}

fn ordered_pair(a: i64, b: i64) -> (i64, i64) {
    (a.min(b), a.max(b))
}

/// Max pairs per `pair_condition` query. Each pair becomes one OR branch;
/// SQLite caps expression tree depth at 1000, so callers with more pairs than
/// this (e.g. a large backfill batch) must chunk — see `pair_rows_chunked`.
const PAIR_QUERY_CHUNK: usize = 400;

/// `WHERE (source_col, id_col) IN (pairs)`, expressed as an OR-of-ANDs since
/// sea-orm's query builder has no composite-tuple `IN`. Cheap for the small,
/// bounded pair sets the focused/online dedup path deals with; each branch
/// hits the composite index created on these column pairs in `MusicDb::new`.
fn pair_condition<C: ColumnTrait>(
    pairs: &[(String, String)],
    source_col: C,
    id_col: C,
) -> Condition {
    let mut cond = Condition::any();
    for (source, identifier) in pairs {
        cond = cond.add(
            Condition::all()
                .add(source_col.eq(source.clone()))
                .add(id_col.eq(identifier.clone())),
        );
    }
    cond
}

/// Runs `query` once per `PAIR_QUERY_CHUNK`-sized slice of `pairs`, concatenating
/// results. `pair_condition` builds one OR branch per pair, so any caller whose
/// pair count isn't already bounded small (i.e. anything besides the focused
/// online-dedup path — batched backfills in particular) must go through this
/// instead of calling `pair_condition` directly.
async fn chunked_pair_query<T, F, Fut>(
    pairs: &[(String, String)],
    query: F,
) -> Result<Vec<T>, Error>
where
    F: Fn(&[(String, String)]) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>, DbErr>>,
{
    let mut out = Vec::new();
    for chunk in pairs.chunks(PAIR_QUERY_CHUNK) {
        out.extend(query(chunk).await?);
    }
    Ok(out)
}

fn source_row_from_model(m: entry_source::Model) -> SourceRow {
    let duration_ms = if let Some(all) = &m.duration_ms_all {
        serde_json::from_str::<Vec<i64>>(all).unwrap_or_default()
    } else {
        m.duration_ms.into_iter().collect()
    };
    SourceRow {
        source: m.source,
        identifier: m.identifier,
        entry_id: m.entry_id,
        duration_ms,
        release_type: m.release_type,
        primary_type: m.primary_type,
        release_date: m.release_date,
    }
}

fn contrib_row_from_model(m: contribution::Model) -> ContribRow {
    ContribRow {
        source: m.source,
        identifier: m.identifier,
        artist_source: m.artist_source,
        artist_identifier: m.artist_identifier,
    }
}

fn child_row_from_model(m: entry_child::Model) -> ChildRow {
    ChildRow {
        parent_source: m.parent_source,
        parent_identifier: m.parent_identifier,
        child_source: m.child_source,
        child_identifier: m.child_identifier,
        disc_no: m.disc_no,
        track_no: m.track_no,
    }
}

struct IdentityDsu {
    parent: HashMap<i64, i64>,
}

impl IdentityDsu {
    fn new(entries: impl IntoIterator<Item = i64>) -> Self {
        Self {
            parent: entries.into_iter().map(|id| (id, id)).collect(),
        }
    }

    fn find(&mut self, id: i64) -> i64 {
        let parent = self.parent.get(&id).copied().unwrap_or(id);
        if parent == id {
            self.parent.entry(id).or_insert(id);
            id
        } else {
            let root = self.find(parent);
            self.parent.insert(id, root);
            root
        }
    }

    fn union(&mut self, a: i64, b: i64) {
        let a = self.find(a);
        let b = self.find(b);
        if a != b {
            let (root, child) = ordered_pair(a, b);
            self.parent.insert(child, root);
        }
    }
}

fn origin_priority(origin: &str) -> u8 {
    match origin {
        "user" | "manual" => 0,
        "imported" | "provider" => 1,
        _ => 2,
    }
}

fn build_soft_identity_projection(
    entry_ids: impl IntoIterator<Item = i64>,
    relations: impl IntoIterator<Item = RelationRow>,
    same_kinds: &[&str],
) -> SoftIdentityProjection {
    let mut dsu = IdentityDsu::new(entry_ids);
    let mut same = Vec::new();
    let mut different = Vec::new();
    for relation in relations.into_iter().filter(|relation| relation.enabled) {
        if same_kinds.contains(&relation.kind.as_str()) {
            same.push(relation);
        } else if relation.kind == DIFFERENT_IDENTITY {
            different.push(ordered_pair(relation.entry_a, relation.entry_b));
        }
    }
    different.sort_unstable();
    different.dedup();
    same.sort_by(|a, b| {
        origin_priority(&a.origin)
            .cmp(&origin_priority(&b.origin))
            .then_with(|| b.confidence.total_cmp(&a.confidence))
            .then_with(|| {
                ordered_pair(a.entry_a, a.entry_b).cmp(&ordered_pair(b.entry_a, b.entry_b))
            })
    });

    let mut conflicts = Vec::new();
    for relation in same {
        let edge = ordered_pair(relation.entry_a, relation.entry_b);
        let left_root = dsu.find(edge.0);
        let right_root = dsu.find(edge.1);
        if left_root == right_root {
            continue;
        }
        let blocker = different.iter().copied().find(|(a, b)| {
            let a_root = dsu.find(*a);
            let b_root = dsu.find(*b);
            (a_root == left_root && b_root == right_root)
                || (a_root == right_root && b_root == left_root)
        });
        if let Some(different_edge) = blocker {
            conflicts.push(SoftIdentityConflict {
                same_edge: edge,
                different_edge,
            });
        } else {
            dsu.union(edge.0, edge.1);
        }
    }

    let ids: Vec<i64> = dsu.parent.keys().copied().collect();
    let mut raw_components: HashMap<i64, Vec<i64>> = HashMap::new();
    for id in ids {
        let root = dsu.find(id);
        raw_components.entry(root).or_default().push(id);
    }
    let mut component_by_entry = HashMap::new();
    let mut members_by_component = BTreeMap::new();
    for mut members in raw_components.into_values() {
        members.sort_unstable();
        let canonical = members[0];
        for &member in &members {
            component_by_entry.insert(member, canonical);
        }
        members_by_component.insert(canonical, members);
    }
    let cannot_components = different
        .into_iter()
        .filter_map(|(a, b)| {
            let pair = ordered_pair(
                component_by_entry.get(&a).copied()?,
                component_by_entry.get(&b).copied()?,
            );
            (pair.0 != pair.1).then_some(pair)
        })
        .collect();
    SoftIdentityProjection {
        component_by_entry,
        members_by_component,
        conflicts,
        cannot_components,
    }
}

/// Combine the `extra` payloads of two relations being merged into one. Both are
/// preserved when present and different: JSON values are unioned into a flat,
/// deduplicated, sorted array (so repeated merges converge), and a lone present
/// payload is kept verbatim. Returns `None` only when both are absent.
///
/// This is the focal point for handling `extra` on a merge and is deliberately
/// kept small so it can later be replaced by richer, possibly user-defined
/// (e.g. Rhai) logic.
fn combine_extra(a: &Option<String>, b: &Option<String>) -> Option<String> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x.clone()),
        (Some(x), Some(y)) if x == y => Some(x.clone()),
        (Some(x), Some(y)) => {
            let mut items: Vec<serde_json::Value> = Vec::new();
            for raw in [x, y] {
                match serde_json::from_str::<serde_json::Value>(raw) {
                    Ok(serde_json::Value::Array(arr)) => items.extend(arr),
                    Ok(v) => items.push(v),
                    Err(_) => items.push(serde_json::Value::String(raw.clone())),
                }
            }
            items.sort_by_key(|v| v.to_string());
            items.dedup_by_key(|v| v.to_string());
            Some(serde_json::Value::Array(items).to_string())
        }
    }
}

/// Combine two relations that share the same `(entry_a, entry_b, kind)` into one.
///
/// This is the single seam where duplicate relations are reconciled when entries
/// merge. The current policy is intentionally simple — keep the higher-confidence
/// relation's scalar fields (deterministic tie-break on origin/enabled) and union
/// the two `extra` payloads via [`combine_extra`]. It is **lossy** on the scalar
/// fields of the discarded relation; callers log when they invoke it. Replace
/// this with richer logic (e.g. a user-provided Rhai hook) when needed.
fn combine_relations(
    a: &entry_relation::Model,
    b: &entry_relation::Model,
) -> entry_relation::Model {
    let a_wins = match a.confidence.partial_cmp(&b.confidence) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Less) => false,
        _ => (&a.origin, a.enabled) >= (&b.origin, b.enabled),
    };
    let primary = if a_wins { a } else { b };
    entry_relation::Model {
        entry_a: primary.entry_a,
        entry_b: primary.entry_b,
        kind: primary.kind.clone(),
        confidence: primary.confidence,
        origin: primary.origin.clone(),
        enabled: primary.enabled,
        extra: combine_extra(&a.extra, &b.extra),
    }
}

/// Re-point every `entry_relation` row referencing `loser` onto `winner` and keep
/// the table referentially consistent. Endpoint order is normalized
/// (`entry_a < entry_b`). The two lossy steps are made explicit with `warn!`: a
/// relation that becomes `winner ↔ winner` (both endpoints merged into one entry)
/// is **dropped**, and rows that collide on `(entry_a, entry_b, kind)` are
/// **merged** into one via [`combine_relations`]. Winner rows are included so a
/// remapped loser relation merges against any pre-existing winner relation.
async fn remap_relations<C: ConnectionTrait>(
    txn: &C,
    loser: i64,
    winner: i64,
) -> Result<(), Error> {
    use entry_relation::Column as Col;

    let touching = Condition::any()
        .add(Col::EntryA.eq(loser))
        .add(Col::EntryB.eq(loser))
        .add(Col::EntryA.eq(winner))
        .add(Col::EntryB.eq(winner));

    let rows = entry_relation::Entity::find()
        .filter(touching.clone())
        .all(txn)
        .await?;
    if rows.is_empty() {
        return Ok(());
    }

    // Delete the affected rows; the canonicalized survivors are re-inserted.
    entry_relation::Entity::delete_many()
        .filter(touching)
        .exec(txn)
        .await?;

    let mut merged: HashMap<(i64, i64, String), entry_relation::Model> = HashMap::new();
    for mut r in rows {
        if r.entry_a == loser {
            r.entry_a = winner;
        }
        if r.entry_b == loser {
            r.entry_b = winner;
        }
        if r.entry_a == r.entry_b {
            // Lossy: a relationship that is now internal to a single entry is
            // dropped. Surface it so the loss is never silent.
            warn!(
                entry = r.entry_a,
                kind = %r.kind,
                confidence = r.confidence,
                origin = %r.origin,
                "merge dropped self-relation (both endpoints merged into one entry)"
            );
            continue;
        }
        if r.entry_a > r.entry_b {
            std::mem::swap(&mut r.entry_a, &mut r.entry_b);
        }
        let key = (r.entry_a, r.entry_b, r.kind.clone());
        match merged.remove(&key) {
            Some(existing) => {
                // Lossy: two relations collapse into one. Surface it.
                warn!(
                    entry_a = key.0,
                    entry_b = key.1,
                    kind = %key.2,
                    "merge combined duplicate relations into one (see combine_relations)"
                );
                merged.insert(key, combine_relations(&existing, &r));
            }
            None => {
                merged.insert(key, r);
            }
        }
    }

    for (_, r) in merged {
        entry_relation::Entity::insert(entry_relation::ActiveModel {
            entry_a: Set(r.entry_a),
            entry_b: Set(r.entry_b),
            kind: Set(r.kind),
            confidence: Set(r.confidence),
            origin: Set(r.origin),
            enabled: Set(r.enabled),
            extra: Set(r.extra),
        })
        .exec(txn)
        .await?;
    }

    Ok(())
}

async fn upsert_relation_on<C: ConnectionTrait>(
    db: &C,
    entry_a: i64,
    entry_b: i64,
    kind: &str,
    confidence: f64,
    origin: &str,
    extra: Option<&str>,
) -> Result<(), Error> {
    let (a, b) = ordered_pair(entry_a, entry_b);
    entry_relation::Entity::insert(entry_relation::ActiveModel {
        entry_a: Set(a),
        entry_b: Set(b),
        kind: Set(kind.to_string()),
        confidence: Set(confidence),
        origin: Set(origin.to_string()),
        enabled: Set(true),
        extra: Set(extra.map(str::to_owned)),
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
        ])
        .to_owned(),
    )
    .exec(db)
    .await?;
    Ok(())
}

async fn set_relation_enabled_on<C: ConnectionTrait>(
    db: &C,
    entry_a: i64,
    entry_b: i64,
    kind: &str,
    enabled: bool,
) -> Result<bool, Error> {
    let (a, b) = ordered_pair(entry_a, entry_b);
    let result = entry_relation::Entity::update_many()
        .col_expr(
            entry_relation::Column::Enabled,
            sea_query::Expr::value(enabled),
        )
        .filter(entry_relation::Column::EntryA.eq(a))
        .filter(entry_relation::Column::EntryB.eq(b))
        .filter(entry_relation::Column::Kind.eq(kind))
        .exec(db)
        .await?;
    Ok(result.rows_affected > 0)
}

impl MusicDb {
    pub async fn new(db_url: &str) -> Result<Self, Error> {
        let db = Database::connect(db_url).await?;
        sea_orm::ConnectionTrait::execute_unprepared(
            &db,
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
        )
        .await?;
        db.get_schema_registry("musiclib_rs::musicdb::*")
            .sync(&db)
            .await?;
        // Pair-keyed tables (entry_alias/contribution/entry_child) have no natural
        // single-column index the entity-model attribute macro can express for a
        // composite lookup, and entry_source.entry_id is looked up far more often
        // than it's written. These indexes let the focused/online dedup path (see
        // pipeline::softmatch::entry_infos_by_ids) fetch exactly the rows it needs
        // instead of scanning full tables into memory.
        sea_orm::ConnectionTrait::execute_unprepared(
            &db,
            "CREATE INDEX IF NOT EXISTS idx_entry_source_entry_id ON entry_source(entry_id);
             CREATE INDEX IF NOT EXISTS idx_entry_alias_pair ON entry_alias(source, identifier);
             CREATE INDEX IF NOT EXISTS idx_contribution_track_pair ON contribution(source, identifier);
             CREATE INDEX IF NOT EXISTS idx_contribution_artist_pair ON contribution(artist_source, artist_identifier);
             CREATE INDEX IF NOT EXISTS idx_entry_child_parent_pair ON entry_child(parent_source, parent_identifier);
             CREATE INDEX IF NOT EXISTS idx_entry_child_child_pair ON entry_child(child_source, child_identifier);",
        )
        .await?;
        Ok(Self { db })
    }

    /// Look up the entry_id grouping a given (source, identifier) pair, if any.
    pub async fn find_entry_id_by_pair(
        &self,
        source: &str,
        identifier: &str,
    ) -> Result<Option<i64>, Error> {
        Ok(
            entry_source::Entity::find_by_id((source.to_string(), identifier.to_string()))
                .one(&self.db)
                .await?
                .map(|m| m.entry_id),
        )
    }

    /// Every `(source, identifier)` pair belonging to one of `entry_ids`. Used
    /// by the lazy dedup pass to re-import only the entries that contain a
    /// declared barrier anchor.
    pub async fn pairs_by_entry_ids(
        &self,
        entry_ids: &[i64],
    ) -> Result<Vec<(String, String)>, Error> {
        if entry_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(entry_source::Entity::find()
            .filter(entry_source::Column::EntryId.is_in(entry_ids.iter().copied()))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| (m.source, m.identifier))
            .collect())
    }

    /// Entry ids with at least one alias containing `query` (case-insensitive
    /// substring match), most recently... actually unordered beyond SQLite's
    /// natural row order — good enough for a bounded typeahead, not a ranked
    /// search. Used by the player's library search endpoint.
    pub async fn search_entry_ids_by_alias(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<i64>, Error> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let pattern = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let stmt = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "SELECT DISTINCT es.entry_id AS entry_id \
             FROM entry_alias ea \
             JOIN entry_source es ON es.source = ea.source AND es.identifier = ea.identifier \
             WHERE ea.name LIKE ? ESCAPE '\\' \
             LIMIT ?",
            [pattern.into(), (limit as i64).into()],
        );
        self.db
            .query_all_raw(stmt)
            .await?
            .iter()
            .map(|row| row.try_get::<i64>("", "entry_id").map_err(Error::from))
            .collect()
    }

    /// Up to `limit` track entry ids chosen uniformly at random — the
    /// backing query for the player's "shuffle the whole library" entry
    /// point. No playability filter here: that needs each entry's (and its
    /// soft-linked siblings') sources, a separate batched lookup the caller
    /// already does via `entry_summaries` — over-fetch and filter there.
    pub async fn random_track_entry_ids(&self, limit: usize) -> Result<Vec<i64>, Error> {
        let stmt = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "SELECT id FROM entry WHERE entry_type = 'track' ORDER BY RANDOM() LIMIT ?",
            [(limit as i64).into()],
        );
        self.db
            .query_all_raw(stmt)
            .await?
            .iter()
            .map(|row| row.try_get::<i64>("", "id").map_err(Error::from))
            .collect()
    }

    /// Allocate a fresh `entry` row and return its auto-assigned id.
    pub async fn insert_entry(&self, entry_type: Option<EntryType>) -> Result<i64, Error> {
        let result = entry::Entity::insert(entry::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            entry_type: Set(entry_type_str(entry_type).to_string()),
        })
        .exec(&self.db)
        .await?;
        Ok(result.last_insert_id)
    }

    /// Update the `entry_type` column for an existing entry.
    pub async fn set_entry_type(&self, entry_id: i64, entry_type: EntryType) -> Result<(), Error> {
        entry::Entity::update_many()
            .col_expr(
                entry::Column::EntryType,
                sea_query::Expr::value(entry_type_str(Some(entry_type))),
            )
            .filter(entry::Column::Id.eq(entry_id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Re-point every `entry_source` row from `loser` to `winner` and delete the
    /// loser `entry` row. No other tables reference `entry_id`, so this is the
    /// entirety of a DB-level entry merge.
    pub async fn merge_entries(&self, loser: i64, winner: i64) -> Result<(), Error> {
        if loser == winner {
            return Ok(());
        }
        let txn = self.db.begin().await?;

        // Re-point all of the loser's source rows to the winner.
        entry_source::Entity::update_many()
            .col_expr(
                entry_source::Column::EntryId,
                sea_query::Expr::value(winner),
            )
            .filter(entry_source::Column::EntryId.eq(loser))
            .exec(&txn)
            .await?;

        // Keep entry_relation referentially consistent: the loser entry is about
        // to be deleted, so re-point every relation that referenced it onto the
        // winner, drop the resulting self-relations, and collapse duplicates.
        remap_relations(&txn, loser, winner).await?;

        // Suggestions are ephemeral review work, not historical evidence.
        // Retire rows involving the disappearing endpoint; dedup_feedback keeps
        // the immutable history with the original ids.
        dedup_suggestion::Entity::update_many()
            .col_expr(
                dedup_suggestion::Column::Status,
                sea_query::Expr::value("superseded_by_hard_merge"),
            )
            .filter(
                Condition::any()
                    .add(dedup_suggestion::Column::EntryA.eq(loser))
                    .add(dedup_suggestion::Column::EntryB.eq(loser)),
            )
            .exec(&txn)
            .await?;

        entry::Entity::delete_by_id(loser).exec(&txn).await?;

        txn.commit().await?;
        Ok(())
    }

    /// Re-point a single existing pair to a different entry. Used by the
    /// importer's split path to move a stub pair (no fresh metadata) off a
    /// contaminated entry; `upsert_pair` already re-points pairs that do carry
    /// fresh metadata. No-op if the pair row doesn't exist.
    pub async fn set_pair_entry(
        &self,
        source: &str,
        identifier: &str,
        entry_id: i64,
    ) -> Result<(), Error> {
        entry_source::Entity::update_many()
            .col_expr(
                entry_source::Column::EntryId,
                sea_query::Expr::value(entry_id),
            )
            .filter(entry_source::Column::Source.eq(source))
            .filter(entry_source::Column::Identifier.eq(identifier))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Delete `entry` rows no longer referenced by any `entry_source` row — e.g.
    /// an entry emptied when a split moved all its pairs elsewhere. Idempotent.
    pub async fn delete_orphan_entries(&self) -> Result<(), Error> {
        let referenced = sea_query::Query::select()
            .distinct()
            .column(entry_source::Column::EntryId)
            .from(entry_source::Entity)
            .to_owned();
        entry::Entity::delete_many()
            .filter(entry::Column::Id.not_in_subquery(referenced))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Insert or update the metadata row for a pair. Updates every metadata
    /// column on conflict — the importer is the only writer and its final
    /// observation wins.
    pub async fn upsert_pair(
        &self,
        source: &str,
        identifier: &str,
        entry_id: i64,
        release_date: Option<&str>,
        specific_data: &EntrySpecificData,
    ) -> Result<(), Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let (durations, release_type, num_discs, num_tracks, primary_type) = match specific_data {
            EntrySpecificData::Track { duration_ms, .. } => {
                (duration_ms.as_slice(), None, None, None, None)
            }
            EntrySpecificData::Release {
                release_type,
                num_discs,
                num_tracks,
            } => (&[][..], release_type.clone(), *num_discs, *num_tracks, None),
            EntrySpecificData::ReleaseGroup { primary_type } => {
                (&[][..], None, None, None, primary_type.clone())
            }
            EntrySpecificData::Artist => (&[][..], None, None, None, None),
        };
        let duration_ms = durations.first().copied();
        let duration_ms_all = if durations.len() > 1 {
            Some(serde_json::to_string(durations).unwrap_or_default())
        } else {
            None
        };

        entry_source::Entity::insert(entry_source::ActiveModel {
            source: Set(source.to_string()),
            identifier: Set(identifier.to_string()),
            entry_id: Set(entry_id),
            release_date: Set(release_date.map(|s| s.to_string())),
            fetched_at: Set(now),
            duration_ms: Set(duration_ms),
            duration_ms_all: Set(duration_ms_all),
            release_type: Set(release_type),
            num_discs: Set(num_discs),
            num_tracks: Set(num_tracks),
            primary_type: Set(primary_type),
        })
        .on_conflict(
            sea_query::OnConflict::columns([
                entry_source::Column::Source,
                entry_source::Column::Identifier,
            ])
            .update_columns([
                entry_source::Column::EntryId,
                entry_source::Column::ReleaseDate,
                entry_source::Column::FetchedAt,
                entry_source::Column::DurationMs,
                entry_source::Column::DurationMsAll,
                entry_source::Column::ReleaseType,
                entry_source::Column::NumDiscs,
                entry_source::Column::NumTracks,
                entry_source::Column::PrimaryType,
            ])
            .to_owned(),
        )
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// Insert a stub row for a pair we never fetched but which is referenced by
    /// an is_rel / has_rel / contribution. Binds the pair to its assigned
    /// `entry_id` so equivalence-class queries see it. Uses `entry_type =
    /// "unknown"` and leaves other metadata NULL. Does nothing on conflict so a
    /// stub will never overwrite a real metadata row.
    pub async fn insert_stub_pair(
        &self,
        source: &str,
        identifier: &str,
        entry_id: i64,
    ) -> Result<(), Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        ignore_not_inserted(
            entry_source::Entity::insert(entry_source::ActiveModel {
                source: Set(source.to_string()),
                identifier: Set(identifier.to_string()),
                entry_id: Set(entry_id),
                release_date: Set(None),
                fetched_at: Set(now),
                duration_ms: Set(None),
                duration_ms_all: Set(None),
                release_type: Set(None),
                num_discs: Set(None),
                num_tracks: Set(None),
                primary_type: Set(None),
            })
            .on_conflict(
                sea_query::OnConflict::columns([
                    entry_source::Column::Source,
                    entry_source::Column::Identifier,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&self.db)
            .await,
        )
    }

    /// Insert aliases for a pair. Caller is responsible for deduping if needed
    /// — the table has no uniqueness constraint on (pair, name, locale).
    pub async fn insert_aliases_for_pair(
        &self,
        source: &str,
        identifier: &str,
        aliases: &[Alias],
    ) -> Result<(), Error> {
        if aliases.is_empty() {
            return Ok(());
        }
        let models: Vec<entry_alias::ActiveModel> = aliases
            .iter()
            .map(|alias| entry_alias::ActiveModel {
                id: sea_orm::ActiveValue::NotSet,
                source: Set(source.to_string()),
                identifier: Set(identifier.to_string()),
                name: Set(alias.name.clone()),
                locale: Set(alias.locale.clone()),
                extra: Set(Some(alias.extra.to_string())),
                primary: Set(alias.primary),
            })
            .collect();
        entry_alias::Entity::insert_many(models)
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Insert a parent→child edge between two pairs. Idempotent on the
    /// composite PK (parent_pair, child_pair).
    pub async fn insert_child_edge(
        &self,
        parent_source: &str,
        parent_identifier: &str,
        child_source: &str,
        child_identifier: &str,
        disc_no: Option<i32>,
        track_no: Option<i32>,
    ) -> Result<(), Error> {
        ignore_not_inserted(
            entry_child::Entity::insert(entry_child::ActiveModel {
                parent_source: Set(parent_source.to_string()),
                parent_identifier: Set(parent_identifier.to_string()),
                child_source: Set(child_source.to_string()),
                child_identifier: Set(child_identifier.to_string()),
                disc_no: Set(disc_no),
                track_no: Set(track_no),
            })
            .on_conflict(
                sea_query::OnConflict::columns([
                    entry_child::Column::ParentSource,
                    entry_child::Column::ParentIdentifier,
                    entry_child::Column::ChildSource,
                    entry_child::Column::ChildIdentifier,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&self.db)
            .await,
        )
    }

    /// Insert a contribution attached to a (parent_pair, artist_pair) edge.
    pub async fn insert_contribution(
        &self,
        source: &str,
        identifier: &str,
        artist_source: &str,
        artist_identifier: &str,
        contrib: &Contribution,
    ) -> Result<(), Error> {
        contribution::Entity::insert(contribution::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            source: Set(source.to_string()),
            identifier: Set(identifier.to_string()),
            artist_source: Set(artist_source.to_string()),
            artist_identifier: Set(artist_identifier.to_string()),
            role: Set(contrib.role.clone()),
            main_artist: Set(contrib.main_artist),
            extra: Set(Some(contrib.extra.to_string())),
        })
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// Last-applied mtime (ns since epoch) of a barrier config file, if recorded.
    pub async fn get_dedup_mtime(&self, config_path: &str) -> Result<Option<i64>, Error> {
        Ok(dedup_state::Entity::find_by_id(config_path.to_string())
            .one(&self.db)
            .await?
            .map(|m| m.mtime_ns))
    }

    /// Record (upsert) the last-applied mtime for a barrier config file.
    pub async fn set_dedup_mtime(&self, config_path: &str, mtime_ns: i64) -> Result<(), Error> {
        dedup_state::Entity::insert(dedup_state::ActiveModel {
            config_path: Set(config_path.to_string()),
            mtime_ns: Set(mtime_ns),
        })
        .on_conflict(
            sea_query::OnConflict::column(dedup_state::Column::ConfigPath)
                .update_column(dedup_state::Column::MtimeNs)
                .to_owned(),
        )
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// Forget a barrier config file's recorded mtime (used when the file is gone).
    pub async fn delete_dedup_mtime(&self, config_path: &str) -> Result<(), Error> {
        dedup_state::Entity::delete_by_id(config_path.to_string())
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Entry ids currently tagged as affected by a given barrier config file.
    pub async fn entries_for_config(&self, config_path: &str) -> Result<Vec<i64>, Error> {
        Ok(entry_dedup::Entity::find()
            .filter(entry_dedup::Column::ConfigPath.eq(config_path))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| m.entry_id)
            .collect())
    }

    /// Every config path that currently tags at least one entry. Used to spot
    /// config files that were removed since the last run.
    pub async fn tagged_config_paths(&self) -> Result<Vec<String>, Error> {
        let mut paths: std::collections::HashSet<String> = std::collections::HashSet::new();
        for m in entry_dedup::Entity::find().all(&self.db).await? {
            paths.insert(m.config_path);
        }
        Ok(paths.into_iter().collect())
    }

    /// Replace the set of entries tagged for `config_path` with `entry_ids`.
    /// Called after every flush so the tags reflect the latest grouping.
    pub async fn set_config_entries(
        &self,
        config_path: &str,
        entry_ids: &[i64],
    ) -> Result<(), Error> {
        entry_dedup::Entity::delete_many()
            .filter(entry_dedup::Column::ConfigPath.eq(config_path))
            .exec(&self.db)
            .await?;
        if entry_ids.is_empty() {
            return Ok(());
        }
        let models: Vec<entry_dedup::ActiveModel> = entry_ids
            .iter()
            .map(|id| entry_dedup::ActiveModel {
                entry_id: Set(*id),
                config_path: Set(config_path.to_string()),
            })
            .collect();
        entry_dedup::Entity::insert_many(models)
            .exec(&self.db)
            .await?;
        Ok(())
    }

    // ── Bulk-fetch helpers (used by the soft-match pipeline) ──────────────────

    /// Every entry row (id, entry_type). Used to seed the match candidate set.
    pub async fn all_entry_rows(&self) -> Result<Vec<EntryRow>, Error> {
        Ok(entry::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| EntryRow {
                id: m.id,
                entry_type: m.entry_type,
            })
            .collect())
    }

    /// Every entry_source row. Used to build alias / metadata views per entry.
    pub async fn all_source_rows(&self) -> Result<Vec<SourceRow>, Error> {
        Ok(entry_source::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(source_row_from_model)
            .collect())
    }

    /// Every alias row. Used to build the alias list per pair.
    pub async fn all_alias_rows(&self) -> Result<Vec<AliasRow>, Error> {
        Ok(entry_alias::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| AliasRow {
                source: m.source,
                identifier: m.identifier,
                name: m.name,
                locale: m.locale,
                primary_alias: m.primary,
            })
            .collect())
    }

    /// Every contribution row. Used to build credited-artist sets per entry.
    pub async fn all_contrib_rows(&self) -> Result<Vec<ContribRow>, Error> {
        Ok(contribution::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(contrib_row_from_model)
            .collect())
    }

    // ── entry_relation ────────────────────────────────────────────────────────

    /// Insert or update a relation between two entries. Enforces `entry_a <
    /// entry_b` so there is exactly one row per unordered pair + kind.
    pub async fn upsert_relation(
        &self,
        entry_a: i64,
        entry_b: i64,
        kind: &str,
        confidence: f64,
        origin: &str,
        extra: Option<&str>,
    ) -> Result<(), Error> {
        upsert_relation_on(&self.db, entry_a, entry_b, kind, confidence, origin, extra).await
    }

    /// Enable or tombstone one exact relation without deleting its provenance.
    /// Returns false when the row did not exist.
    pub async fn set_relation_enabled(
        &self,
        entry_a: i64,
        entry_b: i64,
        kind: &str,
        enabled: bool,
    ) -> Result<bool, Error> {
        set_relation_enabled_on(&self.db, entry_a, entry_b, kind, enabled).await
    }

    /// Project enabled soft-identity assertions into deterministic virtual
    /// components. Cannot-link assertions win; conflicting same-identity edges
    /// are reported rather than silently joining the components.
    pub async fn soft_identity_projection(&self) -> Result<SoftIdentityProjection, Error> {
        self.soft_identity_projection_with_kinds(&[SAME_IDENTITY])
            .await
    }

    /// Like `soft_identity_projection`, but also treats softmatch's own
    /// RELATE output (`entry_relation` rows with kind `"variant"`, written
    /// by `pipeline::softmatch` — see `default_soft_match_config`) as a
    /// same-identity signal, not just a human-confirmed `same_identity` row.
    ///
    /// Deliberately a separate method rather than widening
    /// `soft_identity_projection` itself: that one is also read inside the
    /// scoring pipeline to skip re-scoring pairs it already considers
    /// settled (`are_same` in `score_candidates`), where treating every
    /// heuristic RELATE as permanently confirmed would be the wrong
    /// default. This method is for presentation only — the player unifying
    /// browsing/playback across entries softmatch has already flagged as
    /// likely the same recording, RELATE-tier confidence and all.
    pub async fn soft_identity_projection_for_display(
        &self,
    ) -> Result<SoftIdentityProjection, Error> {
        self.soft_identity_projection_with_kinds(&[SAME_IDENTITY, "variant"])
            .await
    }

    async fn soft_identity_projection_with_kinds(
        &self,
        same_kinds: &[&str],
    ) -> Result<SoftIdentityProjection, Error> {
        let entries = entry::Entity::find().all(&self.db).await?;
        let mut kind_filter =
            Condition::any().add(entry_relation::Column::Kind.eq(DIFFERENT_IDENTITY));
        for kind in same_kinds {
            kind_filter = kind_filter.add(entry_relation::Column::Kind.eq(*kind));
        }
        let relations = entry_relation::Entity::find()
            .filter(entry_relation::Column::Enabled.eq(true))
            .filter(kind_filter)
            .all(&self.db)
            .await?
            .into_iter()
            .map(RelationRow::from)
            .collect::<Vec<_>>();
        Ok(build_soft_identity_projection(
            entries.into_iter().map(|entry| entry.id),
            relations,
            same_kinds,
        ))
    }

    /// Append a user/model judgment and atomically update the active soft
    /// identity relation. `Unsure` acts as a retraction and disables both
    /// identity assertions while retaining all feedback rows.
    pub async fn record_identity_feedback(&self, feedback: NewDedupFeedback) -> Result<i64, Error> {
        if feedback.entry_a == feedback.entry_b {
            return Err(Error::InvalidInput(
                "identity feedback endpoints must differ".into(),
            ));
        }
        if feedback
            .probability
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(Error::InvalidInput(
                "identity feedback probability must be in [0, 1]".into(),
            ));
        }
        let (entry_a, entry_b) = ordered_pair(feedback.entry_a, feedback.entry_b);
        let txn = self.db.begin().await?;
        let left = entry::Entity::find_by_id(entry_a).one(&txn).await?;
        let right = entry::Entity::find_by_id(entry_b).one(&txn).await?;
        let (Some(left), Some(right)) = (left, right) else {
            return Err(Error::InvalidInput(
                "identity feedback references a missing entry".into(),
            ));
        };
        if left.entry_type != right.entry_type {
            return Err(Error::InvalidInput(format!(
                "identity feedback types differ: {:?} and {:?}",
                left.entry_type, right.entry_type
            )));
        }
        if let Some(previous_id) = feedback.supersedes_id {
            let previous = dedup_feedback::Entity::find_by_id(previous_id)
                .one(&txn)
                .await?
                .ok_or_else(|| Error::InvalidInput("superseded feedback does not exist".into()))?;
            if ordered_pair(previous.entry_a, previous.entry_b) != (entry_a, entry_b) {
                return Err(Error::InvalidInput(
                    "superseded feedback belongs to another pair".into(),
                ));
            }
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let relation_origin = feedback.origin.clone();
        let inserted = dedup_feedback::Entity::insert(dedup_feedback::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            entry_a: Set(entry_a),
            entry_b: Set(entry_b),
            judgment: Set(feedback.judgment.as_str().to_owned()),
            origin: Set(feedback.origin),
            model_version: Set(feedback.model_version),
            probability: Set(feedback.probability),
            candidate_channels: Set(feedback.candidate_channels),
            features: Set(feedback.features),
            evidence: Set(feedback.evidence),
            note: Set(feedback.note),
            supersedes_id: Set(feedback.supersedes_id),
            created_at: Set(now),
        })
        .exec(&txn)
        .await?;
        match feedback.judgment {
            IdentityJudgment::Same => {
                upsert_relation_on(
                    &txn,
                    entry_a,
                    entry_b,
                    SAME_IDENTITY,
                    1.0,
                    &relation_origin,
                    None,
                )
                .await?;
                set_relation_enabled_on(&txn, entry_a, entry_b, DIFFERENT_IDENTITY, false).await?;
            }
            IdentityJudgment::Different => {
                upsert_relation_on(
                    &txn,
                    entry_a,
                    entry_b,
                    DIFFERENT_IDENTITY,
                    1.0,
                    &relation_origin,
                    None,
                )
                .await?;
                set_relation_enabled_on(&txn, entry_a, entry_b, SAME_IDENTITY, false).await?;
            }
            IdentityJudgment::Unsure => {
                // A full retraction: clear every kind of active confirmation
                // between this pair, not just `same_identity`/
                // `different_identity`. Without also clearing `"variant"`
                // (the automatic dedup pipeline's own RELATE output —
                // see `soft_identity_projection_for_display`), a pair linked
                // that way would come back enabled the moment this
                // transaction commits: `Unsure` would have touched two rows
                // that were never enabled to begin with and left the one
                // actually holding the pair together untouched.
                set_relation_enabled_on(&txn, entry_a, entry_b, SAME_IDENTITY, false).await?;
                set_relation_enabled_on(&txn, entry_a, entry_b, DIFFERENT_IDENTITY, false).await?;
                set_relation_enabled_on(&txn, entry_a, entry_b, "variant", false).await?;
            }
        }
        dedup_suggestion::Entity::update_many()
            .col_expr(
                dedup_suggestion::Column::Status,
                sea_query::Expr::value("resolved"),
            )
            .col_expr(
                dedup_suggestion::Column::UpdatedAt,
                sea_query::Expr::value(now),
            )
            .filter(dedup_suggestion::Column::EntryA.eq(entry_a))
            .filter(dedup_suggestion::Column::EntryB.eq(entry_b))
            .exec(&txn)
            .await?;
        txn.commit().await?;
        Ok(inserted.last_insert_id)
    }

    /// Complete append-only feedback ledger, in insertion order, for history
    /// views and deterministic offline dataset export.
    pub async fn all_dedup_feedback(&self) -> Result<Vec<DedupFeedbackRow>, Error> {
        Ok(dedup_feedback::Entity::find()
            .order_by_asc(dedup_feedback::Column::Id)
            .all(&self.db)
            .await?
            .into_iter()
            .map(DedupFeedbackRow::from)
            .collect())
    }

    /// Insert or refresh a model suggestion without reopening an already
    /// reviewed row for the same model version.
    pub async fn upsert_dedup_suggestion(
        &self,
        suggestion: NewDedupSuggestion,
    ) -> Result<(), Error> {
        if suggestion.entry_a == suggestion.entry_b
            || !suggestion.probability.is_finite()
            || !(0.0..=1.0).contains(&suggestion.probability)
        {
            return Err(Error::InvalidInput("invalid dedup suggestion".into()));
        }
        let (entry_a, entry_b) = ordered_pair(suggestion.entry_a, suggestion.entry_b);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        dedup_suggestion::Entity::insert(dedup_suggestion::ActiveModel {
            entry_a: Set(entry_a),
            entry_b: Set(entry_b),
            model_version: Set(suggestion.model_version),
            probability: Set(suggestion.probability),
            decision: Set(suggestion.decision),
            candidate_channels: Set(suggestion.candidate_channels),
            features: Set(suggestion.features),
            evidence: Set(suggestion.evidence),
            status: Set("pending".into()),
            created_at: Set(now),
            updated_at: Set(now),
        })
        .on_conflict(
            sea_query::OnConflict::columns([
                dedup_suggestion::Column::EntryA,
                dedup_suggestion::Column::EntryB,
                dedup_suggestion::Column::ModelVersion,
            ])
            .update_columns([
                dedup_suggestion::Column::Probability,
                dedup_suggestion::Column::Decision,
                dedup_suggestion::Column::CandidateChannels,
                dedup_suggestion::Column::Features,
                dedup_suggestion::Column::Evidence,
                dedup_suggestion::Column::UpdatedAt,
            ])
            .to_owned(),
        )
        .exec(&self.db)
        .await?;
        Ok(())
    }

    pub async fn pending_dedup_suggestions(
        &self,
        limit: u64,
    ) -> Result<Vec<DedupSuggestionRow>, Error> {
        Ok(dedup_suggestion::Entity::find()
            .filter(dedup_suggestion::Column::Status.eq("pending"))
            .order_by_desc(dedup_suggestion::Column::Probability)
            .order_by_asc(dedup_suggestion::Column::EntryA)
            .order_by_asc(dedup_suggestion::Column::EntryB)
            .limit(limit)
            .all(&self.db)
            .await?
            .into_iter()
            .map(DedupSuggestionRow::from)
            .collect())
    }

    pub async fn set_dedup_suggestion_status(
        &self,
        entry_a: i64,
        entry_b: i64,
        model_version: &str,
        status: &str,
    ) -> Result<bool, Error> {
        if !matches!(status, "pending" | "snoozed" | "resolved" | "dismissed") {
            return Err(Error::InvalidInput(format!(
                "unsupported dedup suggestion status {status:?}"
            )));
        }
        let (entry_a, entry_b) = ordered_pair(entry_a, entry_b);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let result = dedup_suggestion::Entity::update_many()
            .col_expr(
                dedup_suggestion::Column::Status,
                sea_query::Expr::value(status),
            )
            .col_expr(
                dedup_suggestion::Column::UpdatedAt,
                sea_query::Expr::value(now),
            )
            .filter(dedup_suggestion::Column::EntryA.eq(entry_a))
            .filter(dedup_suggestion::Column::EntryB.eq(entry_b))
            .filter(dedup_suggestion::Column::ModelVersion.eq(model_version))
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected > 0)
    }

    /// Every entry_child row. Used by soft-match to compute release-position features.
    pub async fn all_child_rows(&self) -> Result<Vec<ChildRow>, Error> {
        Ok(entry_child::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(child_row_from_model)
            .collect())
    }

    // ── Scoped bulk-fetch helpers (focused/online soft-match path) ────────────
    //
    // Counterparts to the `all_*_rows` full-table helpers above, filtered to a
    // caller-supplied id/pair set instead of loading the whole library. Used by
    // `pipeline::softmatch::entry_infos_by_ids` to assemble `EntryInfo` for
    // just the entries a candidate search actually touches.

    /// Entry rows for exactly `ids`. Missing ids are silently omitted (e.g. a
    /// stale embedding-cache row pointing at an entry merged away since).
    pub async fn entry_rows_by_ids(&self, ids: &[i64]) -> Result<Vec<EntryRow>, Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(entry::Entity::find()
            .filter(entry::Column::Id.is_in(ids.iter().copied()))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| EntryRow {
                id: m.id,
                entry_type: m.entry_type,
            })
            .collect())
    }

    /// entry_source rows for exactly `ids` (indexed on entry_id).
    pub async fn source_rows_by_entry_ids(&self, ids: &[i64]) -> Result<Vec<SourceRow>, Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(entry_source::Entity::find()
            .filter(entry_source::Column::EntryId.is_in(ids.iter().copied()))
            .all(&self.db)
            .await?
            .into_iter()
            .map(source_row_from_model)
            .collect())
    }

    /// entry_source rows for exactly `pairs` (primary-key lookup).
    pub async fn source_rows_for_pairs(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<SourceRow>, Error> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        Ok(chunked_pair_query(pairs, |chunk| {
            entry_source::Entity::find()
                .filter(pair_condition(
                    chunk,
                    entry_source::Column::Source,
                    entry_source::Column::Identifier,
                ))
                .all(&self.db)
        })
        .await?
        .into_iter()
        .map(source_row_from_model)
        .collect())
    }

    /// entry_alias rows for exactly `pairs` (indexed on (source, identifier)).
    pub async fn alias_rows_for_pairs(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<AliasRow>, Error> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        Ok(chunked_pair_query(pairs, |chunk| {
            entry_alias::Entity::find()
                .filter(pair_condition(
                    chunk,
                    entry_alias::Column::Source,
                    entry_alias::Column::Identifier,
                ))
                .all(&self.db)
        })
        .await?
        .into_iter()
        .map(|m| AliasRow {
            source: m.source,
            identifier: m.identifier,
            name: m.name,
            locale: m.locale,
            primary_alias: m.primary,
        })
        .collect())
    }

    /// contribution rows whose *track* pair is one of `pairs` (indexed).
    pub async fn contrib_rows_for_track_pairs(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<ContribRow>, Error> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        Ok(chunked_pair_query(pairs, |chunk| {
            contribution::Entity::find()
                .filter(pair_condition(
                    chunk,
                    contribution::Column::Source,
                    contribution::Column::Identifier,
                ))
                .all(&self.db)
        })
        .await?
        .into_iter()
        .map(contrib_row_from_model)
        .collect())
    }

    /// contribution rows whose *artist* pair is one of `pairs` (indexed).
    pub async fn contrib_rows_for_artist_pairs(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<ContribRow>, Error> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        Ok(chunked_pair_query(pairs, |chunk| {
            contribution::Entity::find()
                .filter(pair_condition(
                    chunk,
                    contribution::Column::ArtistSource,
                    contribution::Column::ArtistIdentifier,
                ))
                .all(&self.db)
        })
        .await?
        .into_iter()
        .map(contrib_row_from_model)
        .collect())
    }

    /// entry_child rows whose *parent* pair is one of `pairs` (indexed).
    pub async fn child_rows_for_parent_pairs(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<ChildRow>, Error> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        Ok(chunked_pair_query(pairs, |chunk| {
            entry_child::Entity::find()
                .filter(pair_condition(
                    chunk,
                    entry_child::Column::ParentSource,
                    entry_child::Column::ParentIdentifier,
                ))
                .all(&self.db)
        })
        .await?
        .into_iter()
        .map(child_row_from_model)
        .collect())
    }

    /// entry_child rows whose *child* pair is one of `pairs` (indexed).
    pub async fn child_rows_for_child_pairs(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<ChildRow>, Error> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        Ok(chunked_pair_query(pairs, |chunk| {
            entry_child::Entity::find()
                .filter(pair_condition(
                    chunk,
                    entry_child::Column::ChildSource,
                    entry_child::Column::ChildIdentifier,
                ))
                .all(&self.db)
        })
        .await?
        .into_iter()
        .map(child_row_from_model)
        .collect())
    }

    // ── dedup_block_key: persisted candidate-retrieval index ──────────────────

    /// Replace every block-key row for `entry_id` with `keys`. Idempotent and
    /// safe to call again after an entry's aliases/credits/tracklist change —
    /// re-derive the keys and call this again to keep the index current.
    pub async fn replace_block_keys(&self, entry_id: i64, keys: &[String]) -> Result<(), Error> {
        let txn = self.db.begin().await?;
        dedup_block_key::Entity::delete_many()
            .filter(dedup_block_key::Column::EntryId.eq(entry_id))
            .exec(&txn)
            .await?;
        let mut keys = keys.to_vec();
        keys.sort_unstable();
        keys.dedup();
        if !keys.is_empty() {
            dedup_block_key::Entity::insert_many(keys.into_iter().map(|key| {
                dedup_block_key::ActiveModel {
                    entry_id: Set(entry_id),
                    block_key: Set(key),
                }
            }))
            .exec(&txn)
            .await?;
        }
        txn.commit().await?;
        Ok(())
    }

    /// Entry ids sharing `key`, capped at `limit + 1` rows. The extra row over
    /// `limit` is a signal, not data: the caller (mirroring the in-memory
    /// blocker's `max_block` hub cutoff) treats a block this large as too
    /// generic to be useful and skips it entirely rather than truncating it.
    pub async fn block_key_members(&self, key: &str, limit: usize) -> Result<Vec<i64>, Error> {
        Ok(dedup_block_key::Entity::find()
            .filter(dedup_block_key::Column::BlockKey.eq(key))
            .limit(limit as u64 + 1)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| m.entry_id)
            .collect())
    }

    /// Up to `limit` entry ids with no `dedup_block_key` row at all — entries
    /// the focused/online dedup path has never indexed (new library, or one
    /// upgraded from before this index existed). Used by the `dedup` binary's
    /// backfill step to catch the index up in bounded batches; the steady-state
    /// online path never calls this, since it reindexes exactly what it touches.
    pub async fn unindexed_entry_ids(&self, limit: usize) -> Result<Vec<i64>, Error> {
        let indexed = sea_query::Query::select()
            .distinct()
            .column(dedup_block_key::Column::EntryId)
            .from(dedup_block_key::Entity)
            .to_owned();
        Ok(entry::Entity::find()
            .filter(entry::Column::Id.not_in_subquery(indexed))
            .limit(limit as u64)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect())
    }

    /// Every relation row — enabled *or not* — touching `entry_id` or any
    /// live member of its soft-identity component: the graph view the
    /// player renders and lets a human edit directly, including a link a
    /// plain "Unlink" just downgraded to disabled (still shown, dashed, one
    /// click from being restored) and a `different_identity` hard block
    /// (still shown, distinctly, one click from being softened back). Using
    /// only *enabled* edges among *live* members — the obvious-looking
    /// query — would make a downgraded link disappear outright the moment
    /// it's disabled, since disabling it is exactly what drops the far
    /// endpoint out of the live component in the first place; anchoring on
    /// `entry_id` and querying "touches a live member" rather than "both
    /// endpoints are live members" keeps that endpoint's edge (and the node
    /// itself) visible so it stays undoable.
    pub async fn relations_around(&self, entry_id: i64) -> Result<Vec<RelationRow>, Error> {
        let projection = self.soft_identity_projection_for_display().await?;
        let mut anchors: HashSet<i64> = match projection.component_of(entry_id) {
            Some(component) => projection
                .members_by_component
                .get(&component)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            None => HashSet::new(),
        };
        anchors.insert(entry_id);

        Ok(entry_relation::Entity::find()
            .filter(
                Condition::any()
                    .add(entry_relation::Column::EntryA.is_in(anchors.iter().copied()))
                    .add(entry_relation::Column::EntryB.is_in(anchors.iter().copied())),
            )
            .all(&self.db)
            .await?
            .into_iter()
            .map(RelationRow::from)
            .collect())
    }

    /// All enabled relation rows, for reporting.
    pub async fn all_relations(&self) -> Result<Vec<RelationRow>, Error> {
        Ok(entry_relation::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|m| RelationRow {
                entry_a: m.entry_a,
                entry_b: m.entry_b,
                kind: m.kind,
                confidence: m.confidence,
                origin: m.origin,
                enabled: m.enabled,
            })
            .collect())
    }
}

// ── Public data structs for bulk queries ──────────────────────────────────────

pub struct EntryRow {
    pub id: i64,
    pub entry_type: String,
}

pub struct SourceRow {
    pub source: String,
    pub identifier: String,
    pub entry_id: i64,
    /// All known durations for this source, sorted and deduped.
    pub duration_ms: Vec<i64>,
    pub release_type: Option<String>,
    pub primary_type: Option<String>,
    pub release_date: Option<String>,
}

pub struct AliasRow {
    pub source: String,
    pub identifier: String,
    pub name: String,
    pub locale: Option<String>,
    pub primary_alias: bool,
}

pub struct ContribRow {
    pub source: String,
    pub identifier: String,
    pub artist_source: String,
    pub artist_identifier: String,
}

pub struct ChildRow {
    pub parent_source: String,
    pub parent_identifier: String,
    pub child_source: String,
    pub child_identifier: String,
    pub disc_no: Option<i32>,
    pub track_no: Option<i32>,
}

pub struct RelationRow {
    pub entry_a: i64,
    pub entry_b: i64,
    pub kind: String,
    pub confidence: f64,
    pub origin: String,
    pub enabled: bool,
}

impl From<entry_relation::Model> for RelationRow {
    fn from(value: entry_relation::Model) -> Self {
        Self {
            entry_a: value.entry_a,
            entry_b: value.entry_b,
            kind: value.kind,
            confidence: value.confidence,
            origin: value.origin,
            enabled: value.enabled,
        }
    }
}

impl From<dedup_suggestion::Model> for DedupSuggestionRow {
    fn from(value: dedup_suggestion::Model) -> Self {
        Self {
            entry_a: value.entry_a,
            entry_b: value.entry_b,
            model_version: value.model_version,
            probability: value.probability,
            decision: value.decision,
            candidate_channels: value.candidate_channels,
            features: value.features,
            evidence: value.evidence,
            status: value.status,
            updated_at: value.updated_at,
        }
    }
}

impl From<dedup_feedback::Model> for DedupFeedbackRow {
    fn from(value: dedup_feedback::Model) -> Self {
        Self {
            id: value.id,
            entry_a: value.entry_a,
            entry_b: value.entry_b,
            judgment: value.judgment,
            origin: value.origin,
            model_version: value.model_version,
            probability: value.probability,
            candidate_channels: value.candidate_channels,
            features: value.features,
            evidence: value.evidence,
            note: value.note,
            supersedes_id: value.supersedes_id,
            created_at: value.created_at,
        }
    }
}

fn entry_type_str(entry_type: Option<EntryType>) -> &'static str {
    match entry_type {
        Some(EntryType::Artist) => "artist",
        Some(EntryType::ReleaseGroup) => "release_group",
        Some(EntryType::Release) => "release",
        Some(EntryType::Track) => "track",
        None => "unknown",
    }
}

fn ignore_not_inserted<T: sea_orm::ActiveModelTrait>(
    result: Result<sea_orm::InsertResult<T>, DbErr>,
) -> Result<(), Error> {
    match result {
        Ok(_) | Err(DbErr::RecordNotInserted) => Ok(()),
        Err(e) => Err(Error::Database(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_db() -> MusicDb {
        MusicDb::new("sqlite::memory:").await.unwrap()
    }

    async fn insert_entry(db: &DatabaseConnection, id: i64) {
        entry::Entity::insert(entry::ActiveModel {
            id: Set(id),
            entry_type: Set("track".to_string()),
        })
        .exec(db)
        .await
        .unwrap();
    }

    async fn insert_rel(db: &DatabaseConnection, a: i64, b: i64, kind: &str, conf: f64) {
        entry_relation::Entity::insert(entry_relation::ActiveModel {
            entry_a: Set(a),
            entry_b: Set(b),
            kind: Set(kind.to_string()),
            confidence: Set(conf),
            origin: Set("heuristic".to_string()),
            enabled: Set(true),
            extra: Set(None),
        })
        .exec(db)
        .await
        .unwrap();
    }

    /// merge_entries must leave entry_relation referentially consistent: no row
    /// may reference the merged-away loser; a relation that becomes winner↔winner
    /// is dropped; colliding rows are merged into one (higher confidence wins).
    /// Unrelated relations are untouched.
    #[tokio::test]
    async fn merge_entries_keeps_relations_consistent() {
        let mdb = mem_db().await;
        let db = &mdb.db;
        for id in [1i64, 2, 3, 4] {
            insert_entry(db, id).await;
        }
        // (1,3) and (2,3) both "alt" → after 1→2 both become (2,3) → merged.
        insert_rel(db, 1, 3, "alt", 0.9).await;
        insert_rel(db, 2, 3, "alt", 0.5).await;
        // (1,2) → becomes a self-relation after merge → dropped.
        insert_rel(db, 1, 2, "same", 0.8).await;
        // Unrelated relation, must survive verbatim.
        insert_rel(db, 3, 4, "alt", 0.7).await;

        mdb.merge_entries(1, 2).await.unwrap();

        let rows = entry_relation::Entity::find().all(db).await.unwrap();
        assert!(
            rows.iter().all(|r| r.entry_a != 1 && r.entry_b != 1),
            "no relation may reference the merged-away entry 1"
        );
        assert!(
            !rows.iter().any(|r| r.entry_a == r.entry_b),
            "self-relations dropped"
        );
        // Colliding (2,3,alt) merged into one, higher confidence wins.
        let r23: Vec<_> = rows
            .iter()
            .filter(|r| r.entry_a == 2 && r.entry_b == 3 && r.kind == "alt")
            .collect();
        assert_eq!(r23.len(), 1, "duplicate collapsed to one row");
        assert_eq!(r23[0].confidence, 0.9, "higher confidence wins");
        assert!(
            rows.iter()
                .any(|r| r.entry_a == 3 && r.entry_b == 4 && r.kind == "alt"),
            "unrelated relation intact"
        );
        assert!(
            entry::Entity::find_by_id(1)
                .one(db)
                .await
                .unwrap()
                .is_none(),
            "loser entry deleted"
        );
    }

    /// combine_relations is order-independent: the higher-confidence row's scalar
    /// fields win regardless of argument order.
    #[test]
    fn combine_relations_is_order_independent() {
        let mk = |conf: f64, origin: &str| entry_relation::Model {
            entry_a: 2,
            entry_b: 3,
            kind: "alt".to_string(),
            confidence: conf,
            origin: origin.to_string(),
            enabled: true,
            extra: None,
        };
        let a = mk(0.5, "heuristic");
        let b = mk(0.9, "manual");

        let r1 = combine_relations(&a, &b);
        let r2 = combine_relations(&b, &a);
        assert_eq!(r1, r2, "combine is order-independent");
        assert_eq!(r1.confidence, 0.9);
        assert_eq!(r1.origin, "manual", "higher-confidence scalar fields win");
    }

    /// combine_extra preserves both payloads when they differ, keeps a lone one
    /// verbatim, and converges (no growth) when re-combined.
    #[test]
    fn combine_extra_preserves_both_payloads() {
        assert_eq!(combine_extra(&None, &None), None);
        assert_eq!(
            combine_extra(&Some("{\"x\":1}".into()), &None),
            Some("{\"x\":1}".into()),
            "lone payload kept verbatim"
        );

        let a = Some("{\"src\":\"a\"}".to_string());
        let b = Some("{\"src\":\"b\"}".to_string());
        let combined = combine_extra(&a, &b).unwrap();
        let arr: serde_json::Value = serde_json::from_str(&combined).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 2, "both payloads kept");

        // Re-combining with one of the originals must not grow the array.
        let again = combine_extra(&Some(combined.clone()), &a).unwrap();
        let arr2: serde_json::Value = serde_json::from_str(&again).unwrap();
        assert_eq!(arr2.as_array().unwrap().len(), 2, "converges, no growth");
    }

    /// Endpoint order is normalized: a relation stored as (loser, x) with
    /// loser > x must come back as (x, winner) or (winner, x) with entry_a < entry_b.
    #[tokio::test]
    async fn merge_entries_normalizes_endpoint_order() {
        let mdb = mem_db().await;
        let db = &mdb.db;
        for id in [5i64, 10, 20] {
            insert_entry(db, id).await;
        }
        // (5, 20) where 5 will merge into 10 → becomes (10, 20).
        insert_rel(db, 5, 20, "alt", 0.6).await;
        mdb.merge_entries(5, 10).await.unwrap();

        let rows = entry_relation::Entity::find().all(db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].entry_a, rows[0].entry_b), (10, 20));
        assert!(rows[0].entry_a < rows[0].entry_b, "endpoints normalized");
    }

    #[tokio::test]
    async fn soft_identity_edges_are_reversible_and_cannot_link_wins() {
        let mdb = mem_db().await;
        for id in [1i64, 2, 3] {
            insert_entry(&mdb.db, id).await;
        }
        mdb.upsert_relation(1, 2, SAME_IDENTITY, 1.0, "user", None)
            .await
            .unwrap();
        mdb.upsert_relation(2, 3, SAME_IDENTITY, 0.9, "model", None)
            .await
            .unwrap();
        let projection = mdb.soft_identity_projection().await.unwrap();
        assert!(projection.are_same(1, 3));

        assert!(
            mdb.set_relation_enabled(2, 3, SAME_IDENTITY, false)
                .await
                .unwrap()
        );
        let projection = mdb.soft_identity_projection().await.unwrap();
        assert!(projection.are_same(1, 2));
        assert!(!projection.are_same(1, 3));

        mdb.upsert_relation(1, 3, DIFFERENT_IDENTITY, 1.0, "user", None)
            .await
            .unwrap();
        mdb.upsert_relation(2, 3, SAME_IDENTITY, 0.9, "model", None)
            .await
            .unwrap();
        let projection = mdb.soft_identity_projection().await.unwrap();
        assert!(projection.are_same(1, 2));
        assert!(projection.are_different(2, 3));
        assert!(!projection.are_same(1, 3));
        assert_eq!(projection.conflicts.len(), 1);
        assert_eq!(projection.conflicts[0].same_edge, (2, 3));
        assert_eq!(projection.conflicts[0].different_edge, (1, 3));
    }

    #[tokio::test]
    async fn feedback_is_append_only_and_toggles_soft_identity() {
        let mdb = mem_db().await;
        for id in [1i64, 2] {
            insert_entry(&mdb.db, id).await;
        }
        mdb.upsert_dedup_suggestion(NewDedupSuggestion {
            entry_a: 2,
            entry_b: 1,
            model_version: "model-1".into(),
            probability: 0.9,
            decision: "merge".into(),
            candidate_channels: "exact_name".into(),
            features: "{\"name_exact\":1.0}".into(),
            evidence: "{\"left\":{},\"right\":{}}".into(),
        })
        .await
        .unwrap();
        assert_eq!(mdb.pending_dedup_suggestions(10).await.unwrap().len(), 1);

        let same_id = mdb
            .record_identity_feedback(NewDedupFeedback {
                entry_a: 2,
                entry_b: 1,
                judgment: IdentityJudgment::Same,
                origin: "user".into(),
                model_version: Some("model-1".into()),
                probability: Some(0.9),
                candidate_channels: Some("exact_name".into()),
                features: Some("{\"name_exact\":1.0}".into()),
                evidence: Some("{\"left\":{},\"right\":{}}".into()),
                note: None,
                supersedes_id: None,
            })
            .await
            .unwrap();
        assert!(mdb.soft_identity_projection().await.unwrap().are_same(1, 2));
        assert!(mdb.pending_dedup_suggestions(10).await.unwrap().is_empty());

        let different_id = mdb
            .record_identity_feedback(NewDedupFeedback {
                entry_a: 1,
                entry_b: 2,
                judgment: IdentityJudgment::Different,
                origin: "user".into(),
                model_version: Some("model-1".into()),
                probability: Some(0.9),
                candidate_channels: None,
                features: None,
                evidence: None,
                note: Some("wrong artist".into()),
                supersedes_id: Some(same_id),
            })
            .await
            .unwrap();
        let projection = mdb.soft_identity_projection().await.unwrap();
        assert!(!projection.are_same(1, 2));
        assert!(projection.are_different(1, 2));

        mdb.record_identity_feedback(NewDedupFeedback {
            entry_a: 1,
            entry_b: 2,
            judgment: IdentityJudgment::Unsure,
            origin: "user".into(),
            model_version: None,
            probability: None,
            candidate_channels: None,
            features: None,
            evidence: None,
            note: Some("retracted".into()),
            supersedes_id: Some(different_id),
        })
        .await
        .unwrap();
        let projection = mdb.soft_identity_projection().await.unwrap();
        assert!(!projection.are_same(1, 2));
        assert!(!projection.are_different(1, 2));
        let feedback = dedup_feedback::Entity::find().all(&mdb.db).await.unwrap();
        assert_eq!(feedback.len(), 3, "corrections append instead of overwrite");
    }

    #[tokio::test]
    async fn suggestion_refresh_preserves_review_status() {
        let mdb = mem_db().await;
        for id in [1i64, 2] {
            insert_entry(&mdb.db, id).await;
        }
        let suggestion = |probability| NewDedupSuggestion {
            entry_a: 1,
            entry_b: 2,
            model_version: "model-1".into(),
            probability,
            decision: "defer".into(),
            candidate_channels: "char_ngram".into(),
            features: "{}".into(),
            evidence: "{\"left\":{},\"right\":{}}".into(),
        };
        mdb.upsert_dedup_suggestion(suggestion(0.6)).await.unwrap();
        assert!(
            mdb.set_dedup_suggestion_status(2, 1, "model-1", "snoozed")
                .await
                .unwrap()
        );
        mdb.upsert_dedup_suggestion(suggestion(0.7)).await.unwrap();
        assert!(mdb.pending_dedup_suggestions(10).await.unwrap().is_empty());
        let row = dedup_suggestion::Entity::find()
            .one(&mdb.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "snoozed");
        assert_eq!(row.probability, 0.7);
    }

    fn alias(name: &str) -> crate::providers::types::Alias {
        crate::providers::types::Alias {
            name: name.to_string(),
            source: "youtube".to_string(),
            locale: None,
            extra: serde_json::Value::Null,
            primary: true,
        }
    }

    #[tokio::test]
    async fn search_entry_ids_by_alias_matches_substring_case_insensitively() {
        let mdb = mem_db().await;
        let matching = mdb.insert_entry(None).await.unwrap();
        mdb.upsert_pair(
            "youtube",
            "v1",
            matching,
            None,
            &EntrySpecificData::Track {
                duration_ms: vec![],
                positions: Default::default(),
            },
        )
        .await
        .unwrap();
        mdb.insert_aliases_for_pair("youtube", "v1", &[alias("Ridiculous Fervor")])
            .await
            .unwrap();

        let other = mdb.insert_entry(None).await.unwrap();
        mdb.upsert_pair(
            "youtube",
            "v2",
            other,
            None,
            &EntrySpecificData::Track {
                duration_ms: vec![],
                positions: Default::default(),
            },
        )
        .await
        .unwrap();
        mdb.insert_aliases_for_pair("youtube", "v2", &[alias("Totally Different")])
            .await
            .unwrap();

        let ids = mdb
            .search_entry_ids_by_alias("ridiculous", 10)
            .await
            .unwrap();
        assert_eq!(ids, vec![matching]);

        assert!(
            mdb.search_entry_ids_by_alias("", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            mdb.search_entry_ids_by_alias("no such song", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

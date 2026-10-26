//! Batch versions of the writes `pipeline::flush` makes for every pair it
//! saves. Each one loads what's stored for the whole batch at once and skips
//! the writes that would leave a row as it is; the rest go through the
//! single-row writers in `mod.rs`, so the result is the same row for row.
//! An update re-saving 25k unchanged pairs then mostly reads, in a few
//! hundred queries instead of a few hundred thousand.

use std::collections::{HashMap, HashSet};

use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, sea_query};

use super::{
    Error, ImageOrigin, MusicDb, PAIR_QUERY_CHUNK, PairColumns, chunked_id_query,
    chunked_pair_query, contribution, entry, entry_alias, entry_child, entry_source,
    entry_type_str, pair_condition, pair_image, pair_privacy,
};
use crate::providers::types::{Alias, Contribution, EntrySpecificData, EntryType, Image, Privacy};

type Pair = (String, String);

/// Every `E` row whose `(source_col, id_col)` is one of `pairs`.
pub(super) async fn find_by_pairs<E, C>(
    db: &C,
    pairs: &[Pair],
    source_col: E::Column,
    id_col: E::Column,
) -> Result<Vec<E::Model>, Error>
where
    E: EntityTrait,
    C: ConnectionTrait,
    E::Column: ColumnTrait + Copy,
{
    chunked_pair_query(pairs, |chunk| {
        E::find()
            .filter(pair_condition(chunk, source_col, id_col))
            .all(db)
    })
    .await
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A fetched pair to store: `upsert_pair`'s arguments.
pub struct FetchedPair<'a> {
    pub pair: &'a Pair,
    pub entry_id: i64,
    pub release_date: Option<&'a str>,
    pub specific_data: &'a EntrySpecificData,
}

/// A stub pair to store: `upsert_stub_pair`'s arguments.
pub struct StubPair<'a> {
    pub pair: &'a Pair,
    pub entry_id: i64,
    pub duration_ms: Option<i64>,
}

/// A child edge to store: `insert_child_edge`'s arguments.
pub struct NewChildEdge<'a> {
    pub parent: &'a Pair,
    pub child: &'a Pair,
    pub disc_no: Option<i32>,
    pub track_no: Option<i32>,
}

impl MusicDb {
    /// The entry of each of `pairs` that has one.
    pub async fn entry_ids_by_pairs(&self, pairs: &[Pair]) -> Result<HashMap<Pair, i64>, Error> {
        Ok(find_by_pairs::<entry_source::Entity, _>(
            &self.db,
            pairs,
            entry_source::Column::Source,
            entry_source::Column::Identifier,
        )
        .await?
        .into_iter()
        .map(|m| ((m.source, m.identifier), m.entry_id))
        .collect())
    }

    async fn source_models(
        &self,
        pairs: &[Pair],
    ) -> Result<HashMap<Pair, entry_source::Model>, Error> {
        Ok(find_by_pairs::<entry_source::Entity, _>(
            &self.db,
            pairs,
            entry_source::Column::Source,
            entry_source::Column::Identifier,
        )
        .await?
        .into_iter()
        .map(|m| ((m.source.clone(), m.identifier.clone()), m))
        .collect())
    }

    /// `upsert_pair` for each of `rows`. A pair stored with the same entry
    /// and metadata only gets its `fetched_at` moved to now.
    pub async fn upsert_pairs(
        &self,
        rows: &[FetchedPair<'_>],
        run_id: Option<i64>,
    ) -> Result<(), Error> {
        let keys: Vec<Pair> = rows.iter().map(|r| r.pair.clone()).collect();
        let stored = self.source_models(&keys).await?;
        let mut unchanged: Vec<Pair> = Vec::new();
        for r in rows {
            let same = stored.get(r.pair).is_some_and(|m| {
                m.gone_at.is_none()
                    && m.entry_id == r.entry_id
                    && m.release_date.as_deref() == r.release_date
                    && PairColumns::of(r.specific_data)
                        == PairColumns {
                            duration_ms: m.duration_ms,
                            duration_ms_all: m.duration_ms_all.clone(),
                            track_extra: m.track_extra.clone(),
                            release_type: m.release_type.clone(),
                            num_discs: m.num_discs,
                            num_tracks: m.num_tracks,
                            primary_type: m.primary_type.clone(),
                        }
            });
            if same {
                unchanged.push(r.pair.clone());
            } else {
                self.upsert_pair(
                    &r.pair.0,
                    &r.pair.1,
                    r.entry_id,
                    r.release_date,
                    r.specific_data,
                    run_id,
                )
                .await?;
            }
        }
        let now = now_secs();
        for chunk in unchanged.chunks(PAIR_QUERY_CHUNK) {
            entry_source::Entity::update_many()
                .col_expr(entry_source::Column::FetchedAt, sea_query::Expr::value(now))
                .filter(pair_condition(
                    chunk,
                    entry_source::Column::Source,
                    entry_source::Column::Identifier,
                ))
                .exec(&self.db)
                .await?;
        }
        Ok(())
    }

    /// `upsert_stub_pair` for each of `rows`, skipping the ones it would
    /// leave alone: a stored row that was fetched, already has a duration,
    /// or would get none.
    pub async fn upsert_stub_pairs(
        &self,
        rows: &[StubPair<'_>],
        run_id: Option<i64>,
    ) -> Result<(), Error> {
        let keys: Vec<Pair> = rows.iter().map(|r| r.pair.clone()).collect();
        let stored = self.source_models(&keys).await?;
        for r in rows {
            let no_op = stored.get(r.pair).is_some_and(|m| {
                m.fetched_at.is_some() || m.duration_ms.is_some() || r.duration_ms.is_none()
            });
            if !no_op {
                self.upsert_stub_pair(&r.pair.0, &r.pair.1, r.entry_id, r.duration_ms, run_id)
                    .await?;
            }
        }
        Ok(())
    }

    /// `set_pair_image` for each of `rows`, skipping pairs that already
    /// have exactly that image.
    pub async fn set_pair_images(
        &self,
        rows: &[(&Pair, &Image)],
        origin: ImageOrigin,
        run_id: Option<i64>,
    ) -> Result<(), Error> {
        let keys: Vec<Pair> = rows.iter().map(|(p, _)| (*p).clone()).collect();
        let stored: HashMap<Pair, pair_image::Model> = find_by_pairs::<pair_image::Entity, _>(
            &self.db,
            &keys,
            pair_image::Column::Source,
            pair_image::Column::Identifier,
        )
        .await?
        .into_iter()
        .map(|m| ((m.source.clone(), m.identifier.clone()), m))
        .collect();
        for (pair, image) in rows {
            let same = stored.get(*pair).is_some_and(|m| {
                m.url == image.url.as_str()
                    && m.width == image.width.map(|w| w as i32)
                    && m.height == image.height.map(|h| h as i32)
                    && m.origin == origin.as_str()
            });
            if !same {
                self.set_pair_image(&pair.0, &pair.1, image, origin, run_id)
                    .await?;
            }
        }
        Ok(())
    }

    /// `set_provider_privacy` for each of `rows`. Where the provider still
    /// says the same, only `checked_at` moves to now.
    pub async fn set_provider_privacies(&self, rows: &[(&Pair, Privacy)]) -> Result<(), Error> {
        let keys: Vec<Pair> = rows.iter().map(|(p, _)| (*p).clone()).collect();
        let stored: HashMap<Pair, Option<String>> = find_by_pairs::<pair_privacy::Entity, _>(
            &self.db,
            &keys,
            pair_privacy::Column::Source,
            pair_privacy::Column::Identifier,
        )
        .await?
        .into_iter()
        .map(|m| ((m.source, m.identifier), m.provider))
        .collect();
        let mut unchanged: Vec<Pair> = Vec::new();
        for (pair, privacy) in rows {
            if stored
                .get(*pair)
                .is_some_and(|p| p.as_deref() == Some(privacy.as_str()))
            {
                unchanged.push((*pair).clone());
            } else {
                self.set_provider_privacy(&pair.0, &pair.1, *privacy)
                    .await?;
            }
        }
        let now = now_secs();
        for chunk in unchanged.chunks(PAIR_QUERY_CHUNK) {
            pair_privacy::Entity::update_many()
                .col_expr(pair_privacy::Column::CheckedAt, sea_query::Expr::value(now))
                .filter(pair_condition(
                    chunk,
                    pair_privacy::Column::Source,
                    pair_privacy::Column::Identifier,
                ))
                .exec(&self.db)
                .await?;
        }
        Ok(())
    }

    /// `insert_aliases_for_pair` for each of `rows`: only aliases a pair
    /// doesn't have yet are inserted.
    pub async fn insert_aliases_for_pairs(
        &self,
        rows: &[(&Pair, Vec<Alias>)],
        run_id: Option<i64>,
    ) -> Result<(), Error> {
        let keys: Vec<Pair> = rows
            .iter()
            .filter(|(_, aliases)| !aliases.is_empty())
            .map(|(p, _)| (*p).clone())
            .collect();
        let mut seen: HashMap<Pair, HashSet<super::AliasKey>> = HashMap::new();
        for a in find_by_pairs::<entry_alias::Entity, _>(
            &self.db,
            &keys,
            entry_alias::Column::Source,
            entry_alias::Column::Identifier,
        )
        .await?
        {
            let key = super::alias_key(
                &a.name,
                a.locale.as_deref(),
                a.extra.as_deref().unwrap_or("null"),
                a.primary,
            );
            seen.entry((a.source, a.identifier))
                .or_default()
                .insert(key);
        }
        let mut models: Vec<entry_alias::ActiveModel> = Vec::new();
        for (pair, aliases) in rows {
            let seen = seen.entry((*pair).clone()).or_default();
            for alias in aliases {
                let key = super::alias_key(
                    &alias.name,
                    alias.locale.as_deref(),
                    &alias.extra.to_string(),
                    alias.primary,
                );
                if seen.insert(key) {
                    models.push(entry_alias::ActiveModel {
                        id: sea_orm::ActiveValue::NotSet,
                        source: sea_orm::ActiveValue::Set(pair.0.clone()),
                        identifier: sea_orm::ActiveValue::Set(pair.1.clone()),
                        name: sea_orm::ActiveValue::Set(alias.name.clone()),
                        locale: sea_orm::ActiveValue::Set(alias.locale.clone()),
                        extra: sea_orm::ActiveValue::Set(Some(alias.extra.to_string())),
                        primary: sea_orm::ActiveValue::Set(alias.primary),
                        run_id: sea_orm::ActiveValue::Set(run_id),
                    });
                }
            }
        }
        const ALIAS_INSERT_CHUNK: usize = 150;
        while !models.is_empty() {
            let rest = models.split_off(models.len().min(ALIAS_INSERT_CHUNK));
            entry_alias::Entity::insert_many(std::mem::replace(&mut models, rest))
                .exec(&self.db)
                .await?;
        }
        Ok(())
    }

    /// `insert_child_edge` for each of `edges` not stored yet.
    pub async fn insert_child_edges(
        &self,
        edges: &[NewChildEdge<'_>],
        run_id: Option<i64>,
    ) -> Result<(), Error> {
        let parents: Vec<Pair> = unique(edges.iter().map(|e| e.parent));
        let mut stored: HashSet<(Pair, Pair)> = find_by_pairs::<entry_child::Entity, _>(
            &self.db,
            &parents,
            entry_child::Column::ParentSource,
            entry_child::Column::ParentIdentifier,
        )
        .await?
        .into_iter()
        .map(|m| {
            (
                (m.parent_source, m.parent_identifier),
                (m.child_source, m.child_identifier),
            )
        })
        .collect();
        for e in edges {
            if stored.insert((e.parent.clone(), e.child.clone())) {
                self.insert_child_edge(
                    &e.parent.0,
                    &e.parent.1,
                    &e.child.0,
                    &e.child.1,
                    e.disc_no,
                    e.track_no,
                    run_id,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// `insert_contribution` (parent pair, artist pair, credit) for each of
    /// `rows` not stored yet. Like it, a stored row with no `extra` never
    /// counts as the same credit.
    pub async fn insert_contributions(
        &self,
        rows: &[(&Pair, &Pair, &Contribution)],
        run_id: Option<i64>,
    ) -> Result<(), Error> {
        type Key = (Pair, Pair, String, bool, String);
        let parents: Vec<Pair> = unique(rows.iter().map(|(p, _, _)| *p));
        let mut stored: HashSet<Key> = find_by_pairs::<contribution::Entity, _>(
            &self.db,
            &parents,
            contribution::Column::Source,
            contribution::Column::Identifier,
        )
        .await?
        .into_iter()
        .filter_map(|m| {
            Some((
                (m.source, m.identifier),
                (m.artist_source, m.artist_identifier),
                m.role,
                m.main_artist,
                m.extra?,
            ))
        })
        .collect();
        for (parent, artist, contrib) in rows {
            let key: Key = (
                (*parent).clone(),
                (*artist).clone(),
                contrib.role.clone(),
                contrib.main_artist,
                contrib.extra.to_string(),
            );
            if stored.insert(key) {
                self.insert_contribution(
                    &parent.0, &parent.1, &artist.0, &artist.1, contrib, run_id,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// `set_entry_type` for each of `types` whose entry has another type.
    pub async fn set_entry_types(&self, types: &[(i64, EntryType)]) -> Result<(), Error> {
        let ids: Vec<i64> = types.iter().map(|(id, _)| *id).collect();
        let mut stored: HashMap<i64, String> = chunked_id_query(&ids, |chunk| {
            entry::Entity::find()
                .filter(entry::Column::Id.is_in(chunk.iter().copied()))
                .all(&self.db)
        })
        .await?
        .into_iter()
        .map(|m| (m.id, m.entry_type))
        .collect();
        for (id, entry_type) in types {
            let name = entry_type_str(Some(*entry_type));
            if stored.get(id).map(String::as_str) != Some(name) {
                self.set_entry_type(*id, *entry_type).await?;
                stored.insert(*id, name.to_string());
            }
        }
        Ok(())
    }
}

fn unique<'a>(pairs: impl Iterator<Item = &'a Pair>) -> Vec<Pair> {
    let set: HashSet<&Pair> = pairs.collect();
    set.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveValue::Set, EntityTrait, PaginatorTrait};

    use super::*;

    fn pair(id: &str) -> Pair {
        ("youtube".to_string(), id.to_string())
    }

    fn track(duration_ms: i64) -> EntrySpecificData {
        EntrySpecificData::Track {
            duration_ms: vec![duration_ms],
            positions: Default::default(),
            songs: vec![],
        }
    }

    async fn stored(db: &MusicDb, p: &Pair) -> entry_source::Model {
        entry_source::Entity::find_by_id(p.clone())
            .one(&db.db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Saving a pair again as it is only moves `fetched_at`; a changed
    /// pair is written in full.
    #[tokio::test]
    async fn upsert_pairs_rewrites_only_changed_pairs() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let entry = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        let (a, b) = (pair("a"), pair("b"));
        let data = track(1000);
        let row = |p, specific_data| FetchedPair {
            pair: p,
            entry_id: entry,
            release_date: Some("2020"),
            specific_data,
        };
        db.upsert_pairs(&[row(&a, &data), row(&b, &data)], Some(1))
            .await
            .unwrap();
        for p in [&a, &b] {
            entry_source::Entity::update(entry_source::ActiveModel {
                source: Set(p.0.clone()),
                identifier: Set(p.1.clone()),
                fetched_at: Set(Some(0)),
                ..Default::default()
            })
            .exec(&db.db)
            .await
            .unwrap();
        }

        let longer = track(2000);
        db.upsert_pairs(&[row(&a, &data), row(&b, &longer)], Some(2))
            .await
            .unwrap();
        let (a_row, b_row) = (stored(&db, &a).await, stored(&db, &b).await);
        assert!(a_row.fetched_at.unwrap() > 0);
        assert_eq!(a_row.duration_ms, Some(1000));
        assert!(b_row.fetched_at.unwrap() > 0);
        assert_eq!(b_row.duration_ms, Some(2000));
        assert_eq!(b_row.run_id, Some(1), "an update keeps the inserting run");
    }

    /// Edges and credits already stored aren't inserted again, a credit
    /// stored without `extra` doesn't match a new one (like
    /// `insert_contribution`), and a batch's own repeats go in once.
    #[tokio::test]
    async fn child_edges_and_credits_insert_only_missing_rows() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let (album, t1, t2, artist) = (pair("album"), pair("t1"), pair("t2"), pair("artist"));
        let edge = |child| NewChildEdge {
            parent: &album,
            child,
            disc_no: None,
            track_no: Some(1),
        };
        db.insert_child_edges(&[edge(&t1)], None).await.unwrap();
        db.insert_child_edges(&[edge(&t1), edge(&t2), edge(&t2)], None)
            .await
            .unwrap();
        assert_eq!(entry_child::Entity::find().count(&db.db).await.unwrap(), 2);

        let credit = Contribution {
            role: "performer".to_string(),
            main_artist: true,
            extra: serde_json::Value::Null,
            source: "youtube".into(),
        };
        contribution::Entity::insert(contribution::ActiveModel {
            source: Set(album.0.clone()),
            identifier: Set(album.1.clone()),
            artist_source: Set(artist.0.clone()),
            artist_identifier: Set(artist.1.clone()),
            role: Set("performer".to_string()),
            main_artist: Set(true),
            extra: Set(None),
            ..Default::default()
        })
        .exec(&db.db)
        .await
        .unwrap();
        let rows = [(&album, &artist, &credit), (&album, &artist, &credit)];
        db.insert_contributions(&rows, None).await.unwrap();
        db.insert_contributions(&rows, None).await.unwrap();
        assert_eq!(
            contribution::Entity::find().count(&db.db).await.unwrap(),
            2,
            "the NULL-extra row plus one new one"
        );
    }
}

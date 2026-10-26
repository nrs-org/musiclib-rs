//! Playlists: named, ordered lists of tracks the user keeps. Playing one
//! copies it into the playback session's queue; the playlist itself never
//! changes as it plays.
//!
//! An item stores a `(source, identifier)` anchor pair, not an entry id,
//! and finds its entry through `entry_source` whenever it's read, like
//! `preferred_source`. Entry ids don't survive merges and re-import splits;
//! a pair goes wherever its entry goes. An item whose anchor no longer
//! exists keeps the title it was added with, so the list can still say
//! what's missing.

use sea_orm::{ActiveValue::Set, ConnectionTrait, EntityTrait, Statement, Value};

use super::{Error, MusicDb, begin_write, entry_source};

mod playlist {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "playlist")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub name: String,
        /// Unix seconds.
        pub created_at: i64,
        /// Unix seconds: the last rename or change to its items.
        pub updated_at: i64,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

mod playlist_item {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "playlist_item")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        #[sea_orm(indexed)]
        pub playlist_id: i64,
        /// Sort key within the playlist. Removals leave gaps.
        pub position: i64,
        /// The anchor pair (`MusicDb::playlist_anchor`).
        pub source: String,
        pub identifier: String,
        /// The entry's title when it was added.
        pub title: Option<String>,
        /// Unix seconds.
        pub added_at: i64,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// Longest playlist name accepted, in characters.
const MAX_NAME_CHARS: usize = 200;

/// Anchor preference by source, most durable first. A MusicBrainz
/// recording is what identity links usually hang off; an ISRC can be shared
/// by distinct recordings (a remaster), so it's a last resort.
const ANCHOR_SOURCE_ORDER: &[&str] = &[
    "musicbrainz",
    "spotify",
    "youtube",
    "soundcloud",
    "nicovideo",
    "local",
    "discogs",
];

fn anchor_rank(source: &str) -> usize {
    match source {
        "isrc" => usize::MAX,
        s => ANCHOR_SOURCE_ORDER
            .iter()
            .position(|&p| p == s)
            .unwrap_or(ANCHOR_SOURCE_ORDER.len()),
    }
}

fn clean_name(name: &str) -> Result<String, Error> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::InvalidInput("playlist name is empty".into()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(Error::InvalidInput(format!(
            "playlist name is longer than {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_owned())
}

/// One playlist, with how many items it has.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistRow {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub item_count: i64,
}

/// One playlist item, in playlist order. `entry_id` is the entry its anchor
/// pair belongs to now, `None` once the pair is gone.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistItemRow {
    pub id: i64,
    pub source: String,
    pub identifier: String,
    pub title: Option<String>,
    pub added_at: i64,
    pub entry_id: Option<i64>,
}

/// An item to add: its anchor pair and current title.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPlaylistItem {
    pub source: String,
    pub identifier: String,
    pub title: Option<String>,
}

const PLAYLIST_COLUMNS: &str = "p.id, p.name, p.created_at, p.updated_at, \
     (SELECT COUNT(*) FROM playlist_item i WHERE i.playlist_id = p.id) AS item_count";

fn playlist_row(row: &sea_orm::QueryResult) -> Result<PlaylistRow, Error> {
    Ok(PlaylistRow {
        id: row.try_get("", "id")?,
        name: row.try_get("", "name")?,
        created_at: row.try_get("", "created_at")?,
        updated_at: row.try_get("", "updated_at")?,
        item_count: row.try_get("", "item_count")?,
    })
}

impl MusicDb {
    /// The pair a playlist item anchors `entry_id` by: one of its own
    /// sources, by `ANCHOR_SOURCE_ORDER`, then identifier. `None` for an
    /// entry with no sources.
    pub async fn playlist_anchor(&self, entry_id: i64) -> Result<Option<(String, String)>, Error> {
        use sea_orm::{ColumnTrait, QueryFilter};
        let pairs = entry_source::Entity::find()
            .filter(entry_source::Column::EntryId.eq(entry_id))
            .all(&self.db)
            .await?;
        Ok(pairs
            .into_iter()
            .map(|m| (m.source, m.identifier))
            .min_by(|a, b| (anchor_rank(&a.0), a).cmp(&(anchor_rank(&b.0), b))))
    }

    /// Creates an empty playlist; returns its id.
    pub async fn create_playlist(&self, name: &str, now: i64) -> Result<i64, Error> {
        let name = clean_name(name)?;
        let _guard = self.write_lock.lock().await;
        let result = playlist::Entity::insert(playlist::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            name: Set(name),
            created_at: Set(now),
            updated_at: Set(now),
        })
        .exec(&self.db)
        .await?;
        Ok(result.last_insert_id)
    }

    /// Every playlist, most recently changed first.
    pub async fn playlists(&self) -> Result<Vec<PlaylistRow>, Error> {
        let rows = self
            .db
            .query_all_raw(Statement::from_string(
                self.db.get_database_backend(),
                format!("SELECT {PLAYLIST_COLUMNS} FROM playlist p ORDER BY p.updated_at DESC, p.id DESC"),
            ))
            .await?;
        rows.iter().map(playlist_row).collect()
    }

    pub async fn playlist(&self, id: i64) -> Result<Option<PlaylistRow>, Error> {
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!("SELECT {PLAYLIST_COLUMNS} FROM playlist p WHERE p.id = ?"),
                [id.into()],
            ))
            .await?;
        row.as_ref().map(playlist_row).transpose()
    }

    /// Returns false for an unknown playlist.
    pub async fn rename_playlist(&self, id: i64, name: &str, now: i64) -> Result<bool, Error> {
        let name = clean_name(name)?;
        let _guard = self.write_lock.lock().await;
        let result = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE playlist SET name = ?, updated_at = ? WHERE id = ?",
                [name.into(), now.into(), id.into()],
            ))
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes a playlist and its items. Returns false for an unknown one.
    pub async fn delete_playlist(&self, id: i64) -> Result<bool, Error> {
        let _guard = self.write_lock.lock().await;
        let txn = begin_write(&self.db).await?;
        let backend = txn.get_database_backend();
        txn.execute_raw(Statement::from_sql_and_values(
            backend,
            "DELETE FROM playlist_item WHERE playlist_id = ?",
            [id.into()],
        ))
        .await?;
        let result = txn
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "DELETE FROM playlist WHERE id = ?",
                [id.into()],
            ))
            .await?;
        txn.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// A playlist's items in order, each with the entry its anchor is in now.
    pub async fn playlist_items(&self, id: i64) -> Result<Vec<PlaylistItemRow>, Error> {
        let rows = self
            .db
            .query_all_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT i.id, i.source, i.identifier, i.title, i.added_at, s.entry_id
                 FROM playlist_item i
                 LEFT JOIN entry_source s ON s.source = i.source AND s.identifier = i.identifier
                 WHERE i.playlist_id = ?
                 ORDER BY i.position, i.id",
                [id.into()],
            ))
            .await?;
        rows.iter()
            .map(|row| {
                Ok(PlaylistItemRow {
                    id: row.try_get("", "id")?,
                    source: row.try_get("", "source")?,
                    identifier: row.try_get("", "identifier")?,
                    title: row.try_get("", "title")?,
                    added_at: row.try_get("", "added_at")?,
                    entry_id: row.try_get("", "entry_id")?,
                })
            })
            .collect()
    }

    /// Appends `items` to the end of playlist `id`, in order. Returns the
    /// new items' ids, or `None` for an unknown playlist.
    pub async fn add_playlist_items(
        &self,
        id: i64,
        items: &[NewPlaylistItem],
        now: i64,
    ) -> Result<Option<Vec<i64>>, Error> {
        let _guard = self.write_lock.lock().await;
        let txn = begin_write(&self.db).await?;
        let backend = txn.get_database_backend();
        let touched = txn
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE playlist SET updated_at = ? WHERE id = ?",
                [now.into(), id.into()],
            ))
            .await?;
        if touched.rows_affected() == 0 {
            return Ok(None);
        }
        let next: i64 = txn
            .query_one_raw(Statement::from_sql_and_values(
                backend,
                "SELECT COALESCE(MAX(position) + 1, 0) AS next FROM playlist_item WHERE playlist_id = ?",
                [id.into()],
            ))
            .await?
            .map(|row| row.try_get("", "next"))
            .transpose()?
            .unwrap_or(0);
        let mut ids = Vec::with_capacity(items.len());
        for (offset, item) in (0_i64..).zip(items) {
            let result = playlist_item::Entity::insert(playlist_item::ActiveModel {
                id: sea_orm::ActiveValue::NotSet,
                playlist_id: Set(id),
                position: Set(next + offset),
                source: Set(item.source.clone()),
                identifier: Set(item.identifier.clone()),
                title: Set(item.title.clone()),
                added_at: Set(now),
            })
            .exec(&txn)
            .await?;
            ids.push(result.last_insert_id);
        }
        txn.commit().await?;
        Ok(Some(ids))
    }

    /// Removes one item. Returns false if `item_id` isn't in playlist `id`.
    pub async fn remove_playlist_item(
        &self,
        id: i64,
        item_id: i64,
        now: i64,
    ) -> Result<bool, Error> {
        let _guard = self.write_lock.lock().await;
        let txn = begin_write(&self.db).await?;
        let backend = txn.get_database_backend();
        let result = txn
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "DELETE FROM playlist_item WHERE playlist_id = ? AND id = ?",
                [id.into(), item_id.into()],
            ))
            .await?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        txn.execute_raw(Statement::from_sql_and_values(
            backend,
            "UPDATE playlist SET updated_at = ? WHERE id = ?",
            [now.into(), id.into()],
        ))
        .await?;
        txn.commit().await?;
        Ok(true)
    }

    /// Puts playlist `id`'s items in the order of `item_ids`, which must
    /// list each of its items exactly once. Returns false for an unknown
    /// playlist.
    pub async fn reorder_playlist(
        &self,
        id: i64,
        item_ids: &[i64],
        now: i64,
    ) -> Result<bool, Error> {
        let _guard = self.write_lock.lock().await;
        let txn = begin_write(&self.db).await?;
        let backend = txn.get_database_backend();
        let touched = txn
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE playlist SET updated_at = ? WHERE id = ?",
                [now.into(), id.into()],
            ))
            .await?;
        if touched.rows_affected() == 0 {
            return Ok(false);
        }
        let rows = txn
            .query_all_raw(Statement::from_sql_and_values(
                backend,
                "SELECT id FROM playlist_item WHERE playlist_id = ?",
                [id.into()],
            ))
            .await?;
        let mut current: Vec<i64> = rows
            .iter()
            .map(|row| row.try_get("", "id"))
            .collect::<Result<_, _>>()?;
        current.sort_unstable();
        let mut given = item_ids.to_vec();
        given.sort_unstable();
        if current != given {
            return Err(Error::InvalidInput(
                "item_ids must list every item of the playlist exactly once".into(),
            ));
        }
        for (position, &item_id) in (0_i64..).zip(item_ids) {
            txn.execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE playlist_item SET position = ? WHERE id = ?",
                [Value::from(position), item_id.into()],
            ))
            .await?;
        }
        txn.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::types::EntryType;

    async fn mem_db() -> MusicDb {
        MusicDb::new("sqlite::memory:").await.unwrap()
    }

    async fn track(mdb: &MusicDb, pairs: &[(&str, &str)]) -> i64 {
        let id = mdb.insert_entry(Some(EntryType::Track)).await.unwrap();
        for (source, identifier) in pairs {
            mdb.upsert_stub_pair(source, identifier, id, None, None)
                .await
                .unwrap();
        }
        id
    }

    fn item(source: &str, identifier: &str, title: &str) -> NewPlaylistItem {
        NewPlaylistItem {
            source: source.into(),
            identifier: identifier.into(),
            title: Some(title.into()),
        }
    }

    fn order(items: &[PlaylistItemRow]) -> Vec<Option<&str>> {
        items.iter().map(|i| i.title.as_deref()).collect()
    }

    #[tokio::test]
    async fn anchor_prefers_durable_sources_over_isrc() {
        let mdb = mem_db().await;
        let id = track(
            &mdb,
            &[
                ("isrc", "JPX000000001"),
                ("youtube", "https://youtu.be/x"),
                ("musicbrainz", "https://musicbrainz.org/recording/r"),
            ],
        )
        .await;
        assert_eq!(
            mdb.playlist_anchor(id).await.unwrap(),
            Some((
                "musicbrainz".into(),
                "https://musicbrainz.org/recording/r".into()
            ))
        );
        let only_isrc = track(&mdb, &[("isrc", "JPX000000002")]).await;
        assert_eq!(
            mdb.playlist_anchor(only_isrc).await.unwrap(),
            Some(("isrc".into(), "JPX000000002".into()))
        );
        let bare = mdb.insert_entry(Some(EntryType::Track)).await.unwrap();
        assert_eq!(mdb.playlist_anchor(bare).await.unwrap(), None);
    }

    #[tokio::test]
    async fn items_keep_their_order_through_add_remove_and_reorder() {
        let mdb = mem_db().await;
        let pl = mdb.create_playlist("  Road trip ", 100).await.unwrap();
        let ids = mdb
            .add_playlist_items(
                pl,
                &[item("youtube", "a", "A"), item("youtube", "b", "B")],
                110,
            )
            .await
            .unwrap()
            .unwrap();
        mdb.add_playlist_items(pl, &[item("youtube", "c", "C")], 120)
            .await
            .unwrap();
        let items = mdb.playlist_items(pl).await.unwrap();
        assert_eq!(order(&items), [Some("A"), Some("B"), Some("C")]);

        assert!(mdb.remove_playlist_item(pl, ids[1], 130).await.unwrap());
        assert!(!mdb.remove_playlist_item(pl, ids[1], 130).await.unwrap());
        let items = mdb.playlist_items(pl).await.unwrap();
        assert_eq!(order(&items), [Some("A"), Some("C")]);

        let reversed: Vec<i64> = items.iter().rev().map(|i| i.id).collect();
        assert!(mdb.reorder_playlist(pl, &reversed, 140).await.unwrap());
        assert_eq!(
            order(&mdb.playlist_items(pl).await.unwrap()),
            [Some("C"), Some("A")]
        );
        // A reorder has to name every item once.
        assert!(matches!(
            mdb.reorder_playlist(pl, &reversed[..1], 150).await,
            Err(Error::InvalidInput(_))
        ));

        // Appending after a removal still lands at the end.
        mdb.add_playlist_items(pl, &[item("youtube", "d", "D")], 160)
            .await
            .unwrap();
        assert_eq!(
            order(&mdb.playlist_items(pl).await.unwrap()),
            [Some("C"), Some("A"), Some("D")]
        );

        let row = mdb.playlist(pl).await.unwrap().unwrap();
        assert_eq!(
            (row.name.as_str(), row.item_count, row.updated_at),
            ("Road trip", 3, 160)
        );
    }

    #[tokio::test]
    async fn items_follow_their_anchor_through_merges_and_splits() {
        let mdb = mem_db().await;
        let mb = ("musicbrainz", "https://musicbrainz.org/recording/r");
        let yt = ("youtube", "https://youtu.be/x");
        let a = track(&mdb, &[mb]).await;
        let b = track(&mdb, &[yt]).await;
        let pl = mdb.create_playlist("Mix", 0).await.unwrap();
        let (source, identifier) = mdb.playlist_anchor(a).await.unwrap().unwrap();
        mdb.add_playlist_items(
            pl,
            &[NewPlaylistItem {
                source,
                identifier,
                title: Some("Song".into()),
            }],
            0,
        )
        .await
        .unwrap();
        let entry = |mdb: MusicDb| async move { mdb.playlist_items(pl).await.unwrap()[0].entry_id };

        mdb.merge_entries(a, b).await.unwrap();
        assert_eq!(entry(mdb.clone()).await, Some(b));

        // A re-import split moves the anchor to a new entry.
        let c = mdb.insert_entry(Some(EntryType::Track)).await.unwrap();
        mdb.db
            .execute_unprepared(&format!(
                "UPDATE entry_source SET entry_id = {c} WHERE source = '{}'",
                mb.0
            ))
            .await
            .unwrap();
        assert_eq!(entry(mdb.clone()).await, Some(c));

        // A retracted anchor leaves the item, unresolved, with its title.
        mdb.db
            .execute_unprepared("DELETE FROM entry_source WHERE source = 'musicbrainz'")
            .await
            .unwrap();
        let items = mdb.playlist_items(pl).await.unwrap();
        assert_eq!(
            (items[0].entry_id, items[0].title.as_deref()),
            (None, Some("Song"))
        );
    }

    #[tokio::test]
    async fn names_are_trimmed_and_checked_and_deletes_take_the_items() {
        let mdb = mem_db().await;
        assert!(matches!(
            mdb.create_playlist("   ", 0).await,
            Err(Error::InvalidInput(_))
        ));
        let long = "x".repeat(MAX_NAME_CHARS + 1);
        assert!(matches!(
            mdb.create_playlist(&long, 0).await,
            Err(Error::InvalidInput(_))
        ));

        let first = mdb.create_playlist("First", 10).await.unwrap();
        let second = mdb.create_playlist("Second", 20).await.unwrap();
        assert!(mdb.rename_playlist(first, " Renamed ", 30).await.unwrap());
        let names: Vec<String> = mdb
            .playlists()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Renamed", "Second"]);

        mdb.add_playlist_items(second, &[item("youtube", "a", "A")], 40)
            .await
            .unwrap();
        assert!(mdb.delete_playlist(second).await.unwrap());
        assert!(!mdb.delete_playlist(second).await.unwrap());
        assert!(mdb.playlist_items(second).await.unwrap().is_empty());
        assert_eq!(mdb.add_playlist_items(second, &[], 50).await.unwrap(), None);
        assert!(!mdb.rename_playlist(second, "Gone", 50).await.unwrap());
    }
}

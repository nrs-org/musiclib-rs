//! Source-scoped artist credits for NRS. Keep each provider's assertions
//! separate: a listed artist or uploader alone says nothing about whether
//! that artist sang, composed, wrote lyrics, or played an instrument.

use std::collections::{BTreeMap, HashMap, HashSet};

use musiclib_rs::musicdb::MusicDb;
use serde_json::{Value, json};

use crate::entries::{EntrySummary, credit_name, entry_summaries};

use super::{SummaryExtras, URL_PRIORITY, summary_json};

pub(super) struct Credit {
    artist: EntrySummary,
    source: String,
    identifier: String,
    artist_source: String,
    artist_identifier: String,
    role: String,
    main_artist: bool,
    name: String,
    credited_name: Option<String>,
    renamed: bool,
    extra: Value,
}

impl Credit {
    fn artist_id(&self) -> i64 {
        self.artist
            .soft_linked_ids
            .iter()
            .copied()
            .chain([self.artist.entry_id])
            .min()
            .unwrap_or(self.artist.entry_id)
    }
}

/// All roles on the entries' own soft-identity components. In particular,
/// never inherit a release's staff onto its tracks, or a song's performers
/// onto its covers/other editions. All lookups are batched by source pair.
pub(super) async fn load(
    db: &MusicDb,
    entries: &[&EntrySummary],
) -> anyhow::Result<HashMap<i64, Vec<Credit>>> {
    let entries: Vec<_> = entries
        .iter()
        .copied()
        .filter(|e| e.entry_type != "artist")
        .collect();
    let mut pairs: Vec<_> = entries
        .iter()
        .flat_map(|e| &e.sources)
        .map(|s| (s.source.clone(), s.identifier.clone()))
        .collect();
    pairs.sort();
    pairs.dedup();
    let mut rows: Vec<_> = db
        .contrib_rows_for_track_pairs(&pairs)
        .await?
        .into_iter()
        .map(|row| {
            let extra = row
                .extra
                .as_deref()
                .and_then(|extra| serde_json::from_str::<Value>(extra).ok())
                .unwrap_or(Value::Null);
            (row, extra)
        })
        .collect();
    rows.sort_by(|(a, ax), (b, bx)| {
        (&a.source, &a.identifier)
            .cmp(&(&b.source, &b.identifier))
            .then_with(|| {
                ax["index"]
                    .as_i64()
                    .unwrap_or(i64::MAX)
                    .cmp(&bx["index"].as_i64().unwrap_or(i64::MAX))
            })
            .then(a.id.cmp(&b.id))
    });
    let mut artist_pairs: Vec<_> = rows
        .iter()
        .map(|(c, _)| (c.artist_source.clone(), c.artist_identifier.clone()))
        .collect();
    artist_pairs.sort();
    artist_pairs.dedup();
    let artist_of_pair: HashMap<_, _> = db
        .source_rows_for_pairs(&artist_pairs)
        .await?
        .into_iter()
        .map(|s| ((s.source, s.identifier), s.entry_id))
        .collect();
    let candidate_ids: Vec<_> = artist_of_pair.values().copied().collect();
    let artist_ids: Vec<_> = db
        .entry_rows_by_ids(&candidate_ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "artist")
        .map(|e| e.id)
        .collect();
    let artists = entry_summaries(db, &artist_ids).await?;
    let mut member_ids: Vec<_> = entries
        .iter()
        .flat_map(|e| &e.members)
        .map(|m| m.entry_id)
        .collect();
    member_ids.sort_unstable();
    member_ids.dedup();
    let overrides = db.credit_display_names(&member_ids).await?;

    let mut out = HashMap::new();
    for entry in entries {
        let own: HashSet<_> = entry
            .sources
            .iter()
            .map(|s| (s.source.as_str(), s.identifier.as_str()))
            .collect();
        let members: Vec<_> = entry.members.iter().map(|m| m.entry_id).collect();
        let mut credits = Vec::new();
        for (row, extra) in &rows {
            if !own.contains(&(row.source.as_str(), row.identifier.as_str())) {
                continue;
            }
            let Some(artist) = artist_of_pair
                .get(&(row.artist_source.clone(), row.artist_identifier.clone()))
                .and_then(|id| artists.get(id))
            else {
                // Some historical contribution rows point at releases or
                // recordings; they are not evidence of an artist credit.
                continue;
            };
            let credited_name = extra["credited_name"].as_str().filter(|n| !n.is_empty());
            let chosen = credit_name(&overrides, &members, artist);
            credits.push(Credit {
                artist: artist.clone(),
                source: row.source.clone(),
                identifier: row.identifier.clone(),
                artist_source: row.artist_source.clone(),
                artist_identifier: row.artist_identifier.clone(),
                role: row.role.clone(),
                main_artist: row.main_artist,
                name: chosen
                    .or(credited_name)
                    .or(artist.title.as_deref())
                    .unwrap_or_default()
                    .to_owned(),
                credited_name: credited_name.map(str::to_owned),
                renamed: chosen.is_some(),
                extra: extra.clone(),
            });
        }
        out.insert(entry.entry_id, credits);
    }
    Ok(out)
}

/// Each credit carries an artist summary so NRS can recognize/bind the
/// artist without an extra resolve, plus the exact pair behind the claim.
pub(super) fn json(credits: &[Credit]) -> Value {
    let extras = SummaryExtras::default();
    Value::Array(
        credits
            .iter()
            .map(|c| {
                let mut artist = summary_json(&c.artist, &extras);
                artist["store_id"] = json!(c.artist_id().to_string());
                json!({
                    "artist": artist,
                    "name": c.name,
                    "credited_name": c.credited_name,
                    "renamed": c.renamed,
                    "role": c.role,
                    "main_artist": c.main_artist,
                    "source": {"source": c.source, "identifier": c.identifier},
                    "artist_source": {
                        "source": c.artist_source, "identifier": c.artist_identifier,
                    },
                    "extra": c.extra,
                })
            })
            .collect(),
    )
}

/// One provider's main-artist line, in its own credit order. Listed artists
/// win over uploaders; MusicBrainz wins over other catalogs. Combining all
/// providers' names would duplicate artists credited under different names.
pub(super) fn subtitle(credits: &[Credit]) -> Option<String> {
    let mut by_source: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for c in credits.iter().filter(|c| c.main_artist) {
        by_source
            .entry((c.source.as_str(), c.identifier.as_str()))
            .or_default()
            .push(c);
    }
    let (_, list) = by_source.into_iter().min_by_key(|((source, _), list)| {
        (
            list.iter().all(|c| c.role != "listed_artist"),
            list.iter().all(|c| c.role == "uploader"),
            *source != "musicbrainz",
            URL_PRIORITY
                .iter()
                .position(|s| s == source)
                .unwrap_or(URL_PRIORITY.len()),
        )
    })?;
    let listed = list.iter().any(|c| c.role == "listed_artist");
    let list: Vec<_> = list
        .into_iter()
        .filter(|c| !listed || c.role == "listed_artist")
        .collect();
    let text = if listed && list[0].source == "musicbrainz" {
        list.iter()
            .map(|c| {
                format!(
                    "{}{}",
                    c.name,
                    c.extra["joinphrase"].as_str().unwrap_or_default()
                )
            })
            .collect::<String>()
    } else {
        list.iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use musiclib_rs::providers::types::{Alias, Contribution, EntryType};

    use super::*;

    async fn entry(db: &MusicDb, kind: EntryType, pair: (&str, &str), name: &str) -> i64 {
        let id = db.insert_entry(Some(kind)).await.unwrap();
        db.upsert_stub_pair(pair.0, pair.1, id, None, None)
            .await
            .unwrap();
        db.insert_aliases_for_pair(
            pair.0,
            pair.1,
            &[Alias {
                name: name.into(),
                source: pair.0.to_owned().into(),
                primary: true,
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();
        id
    }

    async fn contribute(
        db: &MusicDb,
        pair: (&str, &str),
        artist: (&str, &str),
        role: &str,
        main_artist: bool,
        extra: Value,
    ) {
        db.insert_contribution(
            pair.0,
            pair.1,
            artist.0,
            artist.1,
            &Contribution {
                role: role.into(),
                main_artist,
                source: pair.0.to_owned().into(),
                extra,
            },
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn artist_credits_keep_roles_names_order_and_source_evidence() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let track = ("musicbrainz", "https://musicbrainz.org/recording/track");
        let video = ("youtube", "https://www.youtube.com/watch?v=video");
        let cover = ("local", "local://cover");
        let singer = ("musicbrainz", "https://musicbrainz.org/artist/singer");
        let singer_sp = ("spotify", "https://open.spotify.com/artist/singer");
        let guest = ("musicbrainz", "https://musicbrainz.org/artist/guest");
        let uploader = ("youtube", "https://www.youtube.com/@uploader");
        let track_id = entry(&db, EntryType::Track, track, "Song").await;
        let video_id = entry(&db, EntryType::Track, video, "Song MV").await;
        let cover_id = entry(&db, EntryType::Track, cover, "Cover").await;
        let singer_id = entry(&db, EntryType::Artist, singer, "Singer").await;
        let singer_sp_id = entry(&db, EntryType::Artist, singer_sp, "Singer SP").await;
        let guest_id = entry(&db, EntryType::Artist, guest, "Guest").await;
        entry(&db, EntryType::Artist, uploader, "Channel").await;
        db.upsert_relation(track_id, video_id, "same_identity", 1.0, "test", None)
            .await
            .unwrap();
        db.upsert_relation(singer_id, singer_sp_id, "same_identity", 1.0, "test", None)
            .await
            .unwrap();
        db.upsert_relation(track_id, cover_id, "cover", 1.0, "test", None)
            .await
            .unwrap();

        // Deliberately insert the guest first; source ordering must win.
        contribute(
            &db,
            track,
            guest,
            "listed_artist",
            true,
            json!({"index": 1, "credited_name": "Guest"}),
        )
        .await;
        contribute(
            &db,
            track,
            singer,
            "listed_artist",
            true,
            json!({"index": 0, "credited_name": "Character", "joinphrase": " feat. "}),
        )
        .await;
        contribute(
            &db,
            track,
            singer,
            "instrument",
            false,
            json!({"attributes": ["guitar"], "direction": "backward"}),
        )
        .await;
        contribute(&db, video, uploader, "uploader", true, Value::Null).await;
        // Two source pairs for one artist retain distinct claims and one ID.
        contribute(&db, video, singer_sp, "vocal", false, Value::Null).await;
        contribute(&db, cover, guest, "vocal", false, Value::Null).await;
        // Historical bad rows point at non-artists; do not export them.
        contribute(&db, track, cover, "listed_artist", true, Value::Null).await;

        let summaries = entry_summaries(&db, &[track_id, video_id, cover_id, singer_id])
            .await
            .unwrap();
        let entries: Vec<_> = summaries.values().collect();
        let credits = load(&db, &entries).await.unwrap();
        let rows = json(&credits[&track_id]);
        assert_eq!(rows.as_array().unwrap().len(), 5);
        assert_eq!(rows, json(&credits[&video_id]));
        assert_eq!(rows[0]["name"], "Character");
        assert_eq!(rows[0]["credited_name"], "Character");
        assert_eq!(rows[0]["artist"]["title"], "Singer");
        assert_eq!(rows[0]["artist"]["store_id"], singer_id.to_string());
        assert_eq!(rows[0]["artist"]["canonical_url"], singer.1);
        assert_eq!(rows[0]["artist"]["urls"].as_array().unwrap().len(), 2);
        assert_eq!(
            rows[0]["source"],
            json!({"source": track.0, "identifier": track.1})
        );
        assert_eq!(rows[0]["artist_source"]["identifier"], singer.1);
        assert_eq!(rows[0]["role"], "listed_artist");
        assert_eq!(rows[0]["main_artist"], true);
        assert_eq!(rows[0]["renamed"], false);
        assert_eq!(rows[1]["artist"]["store_id"], guest_id.to_string());
        assert_eq!(rows[2]["extra"]["attributes"], json!(["guitar"]));
        assert_eq!(rows[2]["main_artist"], false);
        assert_eq!(rows[3]["role"], "uploader");
        assert_eq!(rows[3]["credited_name"], Value::Null);
        assert_eq!(rows[4]["artist"]["store_id"], singer_id.to_string());
        assert_eq!(rows[4]["artist_source"]["identifier"], singer_sp.1);
        assert_eq!(
            subtitle(&credits[&track_id]).as_deref(),
            Some("Character feat. Guest")
        );
        assert_eq!(json(&credits[&cover_id]).as_array().unwrap().len(), 1);
        assert_eq!(json(&credits[&cover_id])[0]["role"], "vocal");
        assert!(subtitle(&credits[&cover_id]).is_none());
        assert!(!credits.contains_key(&singer_id));

        // Overrides on either soft-linked member apply to that work only,
        // without replacing the provider's original credited-name evidence.
        db.set_credit_display_name(
            video_id,
            &[track_id, video_id],
            singer_sp_id,
            &[singer_id, singer_sp_id],
            Some("Chosen name"),
        )
        .await
        .unwrap();
        let credits = load(&db, &entries).await.unwrap();
        let rows = json(&credits[&track_id]);
        assert_eq!(rows[0]["name"], "Chosen name");
        assert_eq!(rows[0]["renamed"], true);
        assert_eq!(rows[0]["credited_name"], "Character");
        assert_eq!(rows[4]["name"], "Chosen name");
        assert_eq!(json(&credits[&cover_id])[0]["name"], "Guest");
    }

    #[tokio::test]
    async fn artist_credits_support_legacy_rows_and_empty_entries() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let video = ("youtube", "https://www.youtube.com/watch?v=video");
        let uploader = ("youtube", "https://www.youtube.com/@uploader");
        let video_id = entry(&db, EntryType::Track, video, "Video").await;
        let empty_id = entry(&db, EntryType::Release, ("local", "local://empty"), "Empty").await;
        entry(&db, EntryType::Artist, uploader, "Channel").await;
        contribute(&db, video, uploader, "uploader", true, Value::Null).await;
        let summaries = entry_summaries(&db, &[video_id, empty_id]).await.unwrap();
        let credits = load(&db, &summaries.values().collect::<Vec<_>>())
            .await
            .unwrap();
        assert_eq!(json(&credits[&video_id])[0]["name"], "Channel");
        assert_eq!(json(&credits[&video_id])[0]["extra"], Value::Null);
        assert_eq!(subtitle(&credits[&video_id]).as_deref(), Some("Channel"));
        assert_eq!(json(&credits[&empty_id]), json!([]));
        assert!(subtitle(&credits[&empty_id]).is_none());
    }
}

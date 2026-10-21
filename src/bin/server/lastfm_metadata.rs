//! Scrobble metadata comes from a single catalog pair, never a video's title
//! or uploader. Spotify wins over MusicBrainz within the played identity;
//! edition-group relatives (covers, live takes, instrumentals) are excluded.

use musiclib_rs::musicdb::{AliasRow, MusicDb};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub duration_seconds: f64,
    pub source: String,
    pub identifier: String,
}

impl Metadata {
    pub fn eligible(&self, listened: f64) -> bool {
        self.duration_seconds.is_finite()
            && self.duration_seconds > 30.0
            && listened.is_finite()
            && listened >= (self.duration_seconds / 2.0).min(240.0)
    }
}

fn name(aliases: &[AliasRow], source: &str, identifier: &str) -> Option<String> {
    aliases
        .iter()
        .filter(|a| a.source == source && a.identifier == identifier)
        .filter(|a| !a.name.trim().is_empty())
        .min_by_key(|a| (!a.primary_alias, &a.name))
        .map(|a| a.name.trim().to_owned())
}

pub async fn resolve(db: &MusicDb, entry_id: i64) -> anyhow::Result<Option<Metadata>> {
    let members = crate::entries::component_members(db, entry_id).await?;
    let mut sources = db.source_rows_by_entry_ids(&members).await?;
    sources.retain(|s| matches!(s.source.as_str(), "spotify" | "musicbrainz"));
    sources.sort_by(|a, b| {
        (a.source != "spotify", &a.identifier).cmp(&(b.source != "spotify", &b.identifier))
    });
    for source in sources {
        let Some(ms) = source.duration_ms.iter().copied().filter(|&d| d > 0).max() else {
            continue;
        };
        let pair = (source.source.clone(), source.identifier.clone());
        let aliases = db.alias_rows_for_pairs(std::slice::from_ref(&pair)).await?;
        let Some(track) = name(&aliases, &pair.0, &pair.1) else {
            continue;
        };
        let mut credits = db
            .contrib_rows_for_track_pairs(std::slice::from_ref(&pair))
            .await?;
        // Other MusicBrainz performance relationships are not the recording's
        // listed artist. In particular, never substitute a composer/uploader.
        credits.retain(|c| c.main_artist && c.role == "listed_artist");
        credits.sort_by_key(|c| {
            let extra: serde_json::Value = c
                .extra
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            (extra["index"].as_i64().unwrap_or(i64::MAX), c.id)
        });
        let Some(credit) = credits.first() else {
            continue;
        };
        let extra: serde_json::Value = credit
            .extra
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let artist_aliases = db
            .alias_rows_for_pairs(&[(
                credit.artist_source.clone(),
                credit.artist_identifier.clone(),
            )])
            .await?;
        let artist = extra["credited_name"]
            .as_str()
            .filter(|n| !n.trim().is_empty())
            .map(|n| n.trim().to_owned())
            .or_else(|| {
                name(
                    &artist_aliases,
                    &credit.artist_source,
                    &credit.artist_identifier,
                )
            });
        let Some(artist) = artist else { continue };
        // Album is optional. Only use a release containing this exact catalog
        // pair, not an arbitrary album/playlist from a linked provider.
        let mut parents = db
            .child_rows_for_child_pairs(std::slice::from_ref(&pair))
            .await?;
        parents.retain(|p| p.parent_source == pair.0);
        parents.sort_by(|a, b| a.parent_identifier.cmp(&b.parent_identifier));
        let mut album = None;
        for parent in parents {
            let pp = (parent.parent_source, parent.parent_identifier);
            let rows = db.source_rows_for_pairs(std::slice::from_ref(&pp)).await?;
            let ids: Vec<_> = rows.iter().map(|r| r.entry_id).collect();
            if !db
                .entry_rows_by_ids(&ids)
                .await?
                .iter()
                .any(|e| e.entry_type == "release")
            {
                continue;
            }
            album = name(
                &db.alias_rows_for_pairs(std::slice::from_ref(&pp)).await?,
                &pp.0,
                &pp.1,
            );
            if album.is_some() {
                break;
            }
        }
        return Ok(Some(Metadata {
            artist,
            track,
            album,
            duration_seconds: ms as f64 / 1000.0,
            source: pair.0,
            identifier: pair.1,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use musiclib_rs::providers::types::{Alias, Contribution, EntrySpecificData, EntryType};

    fn metadata(duration: f64) -> Metadata {
        Metadata {
            artist: "Singer".into(),
            track: "Song".into(),
            album: None,
            duration_seconds: duration,
            source: "spotify".into(),
            identifier: "track".into(),
        }
    }

    #[test]
    fn lastfm_threshold_boundaries() {
        assert!(!metadata(30.0).eligible(30.0));
        assert!(!metadata(31.0).eligible(15.49));
        assert!(metadata(31.0).eligible(15.5));
        assert!(!metadata(180.0).eligible(89.9));
        assert!(metadata(180.0).eligible(90.0));
        assert!(!metadata(900.0).eligible(239.9));
        assert!(metadata(900.0).eligible(240.0));
        assert!(!metadata(f64::NAN).eligible(500.0));
    }

    async fn pair(db: &MusicDb, id: i64, source: &str, url: &str, title: &str, ms: i64) {
        db.upsert_pair(
            source,
            url,
            id,
            None,
            &EntrySpecificData::Track {
                duration_ms: vec![ms],
                positions: Default::default(),
                songs: vec![],
            },
            None,
        )
        .await
        .unwrap();
        db.insert_aliases_for_pair(
            source,
            url,
            &[Alias {
                name: title.into(),
                primary: true,
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();
    }

    async fn artist(db: &MusicDb, source: &str, track: &str, who: &str) {
        let id = db.insert_entry(Some(EntryType::Artist)).await.unwrap();
        db.upsert_pair(source, "artist", id, None, &EntrySpecificData::Artist, None)
            .await
            .unwrap();
        db.insert_aliases_for_pair(
            source,
            "artist",
            &[Alias {
                name: who.into(),
                primary: true,
                ..Default::default()
            }],
            None,
        )
        .await
        .unwrap();
        db.insert_contribution(
            source,
            track,
            source,
            "artist",
            &Contribution {
                role: "listed_artist".into(),
                main_artist: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prefers_coherent_spotify_metadata_and_falls_back_to_musicbrainz() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let id = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        pair(
            &db,
            id,
            "youtube",
            "video",
            "Singer – Song [Official MV]",
            220000,
        )
        .await;
        assert_eq!(
            resolve(&db, id).await.unwrap(),
            None,
            "never guess from video titles"
        );
        pair(&db, id, "musicbrainz", "recording", "MB Song", 180000).await;
        artist(&db, "musicbrainz", "recording", "MB Singer").await;
        assert_eq!(
            resolve(&db, id).await.unwrap().unwrap().source,
            "musicbrainz"
        );
        pair(&db, id, "spotify", "track", "Spotify Song", 190000).await;
        assert_eq!(
            resolve(&db, id).await.unwrap().unwrap().source,
            "musicbrainz",
            "incomplete Spotify credit falls back"
        );
        artist(&db, "spotify", "track", "Spotify Singer").await;
        let resolved = resolve(&db, id).await.unwrap().unwrap();
        assert_eq!(
            (
                resolved.track.as_str(),
                resolved.artist.as_str(),
                resolved.duration_seconds
            ),
            ("Spotify Song", "Spotify Singer", 190.0)
        );
    }

    #[tokio::test]
    async fn follows_identity_links_but_preserves_cover_and_instrumental_metadata() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let video = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        let spotify = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        let cover = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        pair(&db, video, "youtube", "mv", "Song [Official MV]", 200000).await;
        pair(&db, spotify, "spotify", "original", "Song", 180000).await;
        artist(&db, "spotify", "original", "Original singer").await;
        db.upsert_relation(video, spotify, "same_identity", 1.0, "manual", None)
            .await
            .unwrap();
        db.upsert_relation(spotify, cover, "cover", 1.0, "manual", None)
            .await
            .unwrap();
        assert_eq!(resolve(&db, video).await.unwrap().unwrap().track, "Song");
        assert_eq!(
            resolve(&db, cover).await.unwrap(),
            None,
            "never borrow the original's metadata for a cover"
        );
        pair(&db, cover, "musicbrainz", "cover", "Song (cover)", 190000).await;
        artist(&db, "musicbrainz", "cover", "Cover singer").await;
        assert_eq!(
            resolve(&db, cover).await.unwrap().unwrap().artist,
            "Cover singer"
        );
        let instrumental = db.insert_entry(Some(EntryType::Track)).await.unwrap();
        db.set_edition_derivation(spotify, instrumental, Some("instrumental"))
            .await
            .unwrap();
        assert_eq!(resolve(&db, instrumental).await.unwrap(), None);
    }
}

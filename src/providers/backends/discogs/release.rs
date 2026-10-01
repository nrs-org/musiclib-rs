use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::discogs::{
        SOURCE,
        canonicalize::{artist_url, master_url, match_release_url, release_url, track_url},
        client::DiscogsClient,
        types::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_MASTER, EXTERNAL_TYPE_TRACK},
    },
    std_values::StandardRoleNames,
    types::{
        Alias, CachedChildSource, ChildRef, Contribution, EntityResult, EntrySpecificData,
        EntryType, Error, TrackPosition,
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseResponse {
    pub id: u64,
    pub title: String,
    pub year: Option<u32>,
    pub released: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    pub master_id: Option<u64>,
    #[serde(default)]
    pub artists: Vec<ReleaseArtist>,
    #[serde(default)]
    pub tracklist: Vec<TracklistEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseArtist {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracklistEntry {
    pub position: String,
    pub title: String,
    pub duration: Option<String>,
    #[serde(default)]
    pub artists: Vec<ReleaseArtist>,
    #[serde(default)]
    pub extraartists: Vec<ReleaseArtist>,
    #[serde(rename = "type_")]
    #[serde(default)]
    pub track_type: String,
}

// --- Helpers ---

fn parse_release_date(released: Option<&str>, year: Option<u32>) -> Option<String> {
    if let Some(r) = released {
        let parts: Vec<&str> = r.split('-').collect();
        return Some(match parts.len() {
            3 => format!("{} XX:XX:XX", r),
            2 => format!("{}-XX XX:XX:XX", r),
            1 if !r.is_empty() => format!("{}-XX-XX XX:XX:XX", r),
            _ => return None,
        });
    }
    year.map(|y| format!("{y}-XX-XX XX:XX:XX"))
}

/// Parse "m:ss" or "h:mm:ss" duration string into milliseconds.
pub(crate) fn parse_duration_ms(duration: &str) -> Option<i64> {
    let parts: Vec<&str> = duration.split(':').collect();
    match parts.len() {
        2 => {
            let m: i64 = parts[0].parse().ok()?;
            let s: i64 = parts[1].parse().ok()?;
            Some((m * 60 + s) * 1000)
        }
        3 => {
            let h: i64 = parts[0].parse().ok()?;
            let m: i64 = parts[1].parse().ok()?;
            let s: i64 = parts[2].parse().ok()?;
            Some((h * 3600 + m * 60 + s) * 1000)
        }
        _ => None,
    }
}

/// Parse a Discogs track position string into (disc_no, track_no).
/// Returns None if the position can't be parsed as a numbered track.
pub(crate) fn parse_track_position(position: &str) -> Option<TrackPosition> {
    let s = position.trim();
    if s.is_empty() {
        return None;
    }

    // Pure numeric: "1", "12"
    if let Ok(n) = s.parse::<i32>() {
        return Some(TrackPosition {
            disc_no: None,
            track_no: n,
            synthetic: false,
        });
    }

    // Vinyl side: "A1", "B3", "AA1" etc. — letter(s) then digits
    if let Some(digit_start) = s.find(|c: char| c.is_ascii_digit()) {
        let letters = &s[..digit_start];
        let digits = &s[digit_start..];
        if !letters.is_empty()
            && letters.chars().all(|c| c.is_ascii_alphabetic())
            && let Ok(track_no) = digits.parse::<i32>()
        {
            // Convert letter(s) to disc number: A=1, B=2, ... Z=26, AA=27 ...
            let disc_no = letters.chars().fold(0i32, |acc, c| {
                acc * 26 + (c.to_ascii_uppercase() as i32 - 'A' as i32 + 1)
            });
            return Some(TrackPosition {
                disc_no: Some(disc_no),
                track_no,
                synthetic: false,
            });
        }
    }

    // Disc-track with separator: "1-1", "2-3", "1.1", "2.3"
    for sep in ['-', '.'] {
        if let Some(idx) = s.find(sep) {
            let left = &s[..idx];
            let right = &s[idx + 1..];
            if let (Ok(disc), Ok(track)) = (left.parse::<i32>(), right.parse::<i32>()) {
                return Some(TrackPosition {
                    disc_no: Some(disc),
                    track_no: track,
                    synthetic: false,
                });
            }
        }
    }

    None
}

pub(crate) fn artist_child_refs(artists: &[ReleaseArtist]) -> Vec<ChildRef> {
    artists
        .iter()
        .enumerate()
        .map(|(i, a)| ChildRef {
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
            sources: [(
                SOURCE.into(),
                HashSet::from([artist_url(&a.id.to_string())]),
            )]
            .into(),
            name: Some(a.name.clone()),
            contributions: vec![Contribution {
                role: if a.role.is_empty() {
                    StandardRoleNames::LISTED_ARTIST.into()
                } else {
                    a.role.clone()
                },
                main_artist: a.role.is_empty(),
                source: SOURCE.into(),
                extra: serde_json::json!({ "index": i }),
            }],
            ..Default::default()
        })
        .collect()
}

// --- Public API ---

pub async fn get_release_raw<T, F, R, E, FR>(
    client: &DiscogsClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_release_url(url).expect("Invalid Discogs release URL");
    client
        .get::<T, _, _, _, _>(&format!("releases/{id}"), &[], |value| callback(value, id))
        .await
}

pub async fn get_release(client: &DiscogsClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_release_raw::<ReleaseResponse, _, _, Error, _>(client, url, |r, id| {
        let canonical = release_url(id);

        let release_date = parse_release_date(r.released.as_deref(), r.year);

        let artist_refs = artist_child_refs(&r.artists);

        // Derive disc count from track positions.
        let mut max_disc: i32 = 1;
        let track_refs: Vec<ChildRef> = r
            .tracklist
            .iter()
            .filter(|t| t.track_type != "heading")
            .enumerate()
            .map(|(i, t)| {
                let position = parse_track_position(&t.position);
                if let Some(ref pos) = position
                    && let Some(d) = pos.disc_no
                {
                    max_disc = max_disc.max(d);
                }
                // Main artists: per-track if present, otherwise release-level.
                let main_artists = if !t.artists.is_empty() {
                    artist_child_refs(&t.artists)
                } else {
                    artist_child_refs(&r.artists)
                };
                let extra_artists = artist_child_refs(&t.extraartists);
                let contributions = main_artists
                    .iter()
                    .chain(extra_artists.iter())
                    .flat_map(|c| c.contributions.clone())
                    .collect();
                // Fall back to an index-based synthetic position (disc_no=0 is the sentinel)
                // when the Discogs position string can't be parsed (e.g. "Video", "").
                let effective_pos = position.clone().unwrap_or(TrackPosition {
                    disc_no: None,
                    track_no: (i + 1) as i32,
                    synthetic: true,
                });
                ChildRef {
                    entry_type: EntryType::Track,
                    external_type: EXTERNAL_TYPE_TRACK.into(),
                    sources: [(
                        SOURCE.into(),
                        HashSet::from([track_url(id, &effective_pos)]),
                    )]
                    .into(),
                    name: Some(t.title.clone()),
                    duration_ms: t.duration.as_deref().and_then(parse_duration_ms),
                    appears_on: false,
                    position: Some(effective_pos),
                    contributions,
                    original_relation_kind: None,
                }
            })
            .collect();

        let num_tracks = track_refs.len() as i32;
        let num_discs = max_disc;

        let sources: crate::providers::types::ExternalSources =
            [(SOURCE.into(), HashSet::from([canonical]))].into();

        let mut children: Vec<Arc<CachedChildSource<()>>> = vec![
            Arc::new(CachedChildSource::from_children(artist_refs)),
            Arc::new(CachedChildSource::from_children(track_refs)),
        ];

        // Link to master release group if present.
        if let Some(master_id) = r.master_id {
            let master_ref = ChildRef {
                entry_type: EntryType::ReleaseGroup,
                external_type: EXTERNAL_TYPE_MASTER.into(),
                sources: [(
                    SOURCE.into(),
                    HashSet::from([master_url(&master_id.to_string())]),
                )]
                .into(),
                name: None,
                ..Default::default()
            };
            children.push(Arc::new(CachedChildSource::from_children(vec![master_ref])));
        }

        let result = Ok(EntityResult {
            release_date,
            sources,
            extra: r.extra.clone(),
            specific_data: EntrySpecificData::Release {
                release_type: r.status.clone(),
                num_discs: Some(num_discs),
                num_tracks: Some(num_tracks),
            },
            children,
            aliases: vec![Alias {
                name: r.title.clone(),
                source: SOURCE.into(),
                primary: true,
                ..Default::default()
            }],
        });
        async move { result }
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::{collections::HashSet, sync::Arc};

    use http::Method;

    use crate::{
        providers::{
            backends::discogs::{
                SOURCE,
                client::DiscogsClient,
                release::{ReleaseResponse, get_release},
                types::EXTERNAL_TYPE_TRACK,
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn release_api_url(id: &str) -> String {
        DiscogsClient::build_url(&format!("releases/{id}"), &[])
    }

    #[tokio::test]
    async fn test_get_release_hop_step_sheep() -> anyhow::Result<()> {
        let id = "34963553";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ReleaseResponse>(
            Method::GET,
            &release_api_url(id),
            include_str!("./release_hop_step_sheep.json"),
        );

        let client = DiscogsClient::new_with_client(Arc::new(http_client), None)?;
        let rel = get_release(&client, &format!("https://www.discogs.com/release/{id}")).await?;

        // Release date
        assert_eq!(rel.release_date.as_deref(), Some("2024-01-10 XX:XX:XX"));

        // Alias
        assert_eq!(rel.aliases.len(), 1);
        assert_eq!(rel.aliases[0].name, "Hop Step Sheep");
        assert!(rel.aliases[0].primary);

        // Sources
        assert_eq!(
            rel.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://www.discogs.com/release/34963553".to_string()])
        );

        // Specific data: 1 disc, 10 tracks, status "Accepted"
        assert!(matches!(
            rel.specific_data,
            EntrySpecificData::Release {
                num_discs: Some(1),
                num_tracks: Some(10),
                release_type: Some(ref s),
            } if s == "Accepted"
        ));

        // 2 child sources: artists, tracks (no master_id on this release)
        assert_eq!(rel.children.len(), 2);

        // Artist child
        let mut artists = rel.children[0].cursor();
        let (artist, _) = child_next(&mut artists).await?.expect("expected artist");
        assert_eq!(artist.entry_type, EntryType::Artist);
        assert_eq!(artist.name.as_deref(), Some("角巻わため"));
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://www.discogs.com/artist/11811530".to_string()])
        );
        assert!(child_next(&mut artists).await?.is_none());

        // Track children: 10 tracks
        let mut tracks = rel.children[1].cursor();
        let (track1, _) = child_next(&mut tracks).await?.expect("expected track 1");
        assert_eq!(track1.entry_type, EntryType::Track);
        assert_eq!(track1.external_type.as_ref(), EXTERNAL_TYPE_TRACK);
        assert_eq!(track1.name.as_deref(), Some("Beautiful Circle"));
        let pos = track1.position.as_ref().expect("expected position");
        assert_eq!(pos.track_no, 1);
        assert_eq!(pos.disc_no, None);
        assert_eq!(
            track1.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://www.discogs.com/release/34963553?track=1".to_string()])
        );

        // Last track
        for _ in 2..=9 {
            child_next(&mut tracks).await?.expect("expected track");
        }
        let (track10, _) = child_next(&mut tracks).await?.expect("expected track 10");
        assert_eq!(track10.name.as_deref(), Some("Happy day to you!"));
        let pos10 = track10.position.as_ref().expect("expected position");
        assert_eq!(pos10.track_no, 10);
        assert!(child_next(&mut tracks).await?.is_none());

        Ok(())
    }
}

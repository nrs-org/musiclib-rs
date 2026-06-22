use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::spotify::{
        SOURCE,
        canonicalize::{album_url, artist_url, match_track_url, track_url},
        client::SpotifyClient,
        types::{EXTERNAL_TYPE_ALBUM, EXTERNAL_TYPE_ARTIST, ExternalUrls, SimpleArtist},
    },
    std_values::StandardRoleNames,
    types::{
        Alias, CachedChildSource, ChildRef, Contribution, EntityResult, EntrySpecificData,
        EntryType, Error, TrackPosition,
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TrackResponse {
    pub id: String,
    pub name: String,
    pub duration_ms: i64,
    pub track_number: i32,
    pub disc_number: i32,
    pub explicit: bool,
    pub artists: Vec<SimpleArtist>,
    pub album: Option<SimpleAlbum>,
    #[serde(default)]
    pub external_ids: ExternalIds,
    #[serde(default)]
    pub external_urls: ExternalUrls,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleAlbum {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub external_urls: ExternalUrls,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExternalIds {
    pub isrc: Option<String>,
    pub ean: Option<String>,
    pub upc: Option<String>,
}

// --- Public API ---

pub async fn get_track_raw<T, F, R, E, FR>(
    client: &SpotifyClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_track_url(url).expect("Invalid Spotify track URL");
    client
        .get::<T, _, _, _, _>(&format!("tracks/{id}"), &[], |value| callback(value, id))
        .await
}

pub async fn get_track(client: &SpotifyClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_track_raw::<TrackResponse, _, _, Error, _>(client, url, |t, id| {
        let canonical = track_url(id);

        let mut sources: crate::providers::types::ExternalSources =
            [(SOURCE.into(), HashSet::from([canonical]))].into();
        if let Some(isrc) = &t.external_ids.isrc {
            sources
                .0
                .entry("isrc".into())
                .or_default()
                .insert(isrc.clone());
        }
        if let Some(ean) = &t.external_ids.ean {
            sources
                .0
                .entry("ean".into())
                .or_default()
                .insert(ean.clone());
        }
        if let Some(upc) = &t.external_ids.upc {
            sources
                .0
                .entry("upc".into())
                .or_default()
                .insert(upc.clone());
        }

        let artist_refs: Vec<ChildRef> = t
            .artists
            .iter()
            .enumerate()
            .map(|(i, a)| ChildRef {
                entry_type: EntryType::Artist,
                external_type: EXTERNAL_TYPE_ARTIST.into(),
                sources: [(SOURCE.into(), HashSet::from([artist_url(&a.id)]))].into(),
                name: Some(a.name.clone()),
                contributions: vec![Contribution {
                    role: StandardRoleNames::LISTED_ARTIST.into(),
                    main_artist: true,
                    source: SOURCE.into(),
                    extra: serde_json::json!({ "index": i }),
                }],
                ..Default::default()
            })
            .collect();

        let album_refs: Vec<ChildRef> = t
            .album
            .iter()
            .map(|a| ChildRef {
                entry_type: EntryType::Release,
                external_type: EXTERNAL_TYPE_ALBUM.into(),
                sources: [(SOURCE.into(), HashSet::from([album_url(&a.id)]))].into(),
                name: Some(a.name.clone()),
                ..Default::default()
            })
            .collect();

        let position = Some(TrackPosition {
            disc_no: if t.disc_number > 1 {
                Some(t.disc_number)
            } else {
                None
            },
            track_no: t.track_number,
            synthetic: false,
        });

        let result = Ok(EntityResult {
            release_date: None,
            sources,
            extra: t.extra.clone(),
            specific_data: EntrySpecificData::Track {
                duration_ms: vec![t.duration_ms],
                positions: [(SOURCE.into(), position.clone().unwrap())].into(),
            },
            children: vec![
                Arc::new(CachedChildSource::from_children(artist_refs)),
                Arc::new(CachedChildSource::from_children(album_refs)),
            ],
            aliases: vec![Alias {
                name: t.name.clone(),
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
    use std::sync::Arc;

    use http::Method;

    use crate::{
        providers::{
            backends::spotify::{
                SOURCE,
                client::SpotifyClient,
                track::{TrackResponse, get_track},
                types::EXTERNAL_TYPE_ARTIST,
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn track_api_url(id: &str) -> String {
        SpotifyClient::build_url(&format!("tracks/{id}"), &[])
    }

    #[tokio::test]
    async fn test_get_track() -> anyhow::Result<()> {
        let id = "4C3klfwXqCFv61VJ4bP7oJ";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<TrackResponse>(
            Method::GET,
            &track_api_url(id),
            include_str!("./track_beautiful_circle.json"),
        );

        let client = SpotifyClient::new_with_token(Arc::new(http_client), "test_token");
        let track = get_track(&client, &format!("https://open.spotify.com/track/{id}")).await?;

        // Title and duration
        assert_eq!(track.aliases.len(), 1);
        assert_eq!(track.aliases[0].name, "Beautiful Circle");
        assert!(track.aliases[0].primary);
        assert!(matches!(
            track.specific_data,
            EntrySpecificData::Track {
                ref duration_ms,
                ..
            } if duration_ms == &vec![229233_i64]
        ));

        // Sources: canonical URL + ISRC
        assert_eq!(
            track.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([format!("https://open.spotify.com/track/{id}")])
        );
        assert_eq!(
            track.sources.get("isrc").unwrap(),
            &std::collections::HashSet::from(["JPV752307972".to_string()])
        );

        // Track position: track 1, single disc (disc_no = None)
        let positions = match &track.specific_data {
            EntrySpecificData::Track { positions, .. } => positions,
            _ => panic!("expected Track"),
        };
        let pos = positions.get(SOURCE).expect("expected spotify position");
        assert_eq!(pos.track_no, 1);
        assert_eq!(pos.disc_no, None);

        // children[0] = artists, children[1] = album
        assert_eq!(track.children.len(), 2);

        let mut artists = track.children[0].cursor();
        let (artist, _) = child_next(&mut artists).await?.expect("expected artist");
        assert_eq!(artist.entry_type, EntryType::Artist);
        assert_eq!(artist.external_type.as_ref(), EXTERNAL_TYPE_ARTIST);
        assert_eq!(artist.name.as_deref(), Some("角巻わため"));
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://open.spotify.com/artist/68609MOnEU86kVyMf26JnM".to_string()
            ])
        );

        let mut albums = track.children[1].cursor();
        let (album, _) = child_next(&mut albums)
            .await?
            .expect("expected parent album");
        assert_eq!(album.entry_type, EntryType::Release);
        assert_eq!(album.name.as_deref(), Some("Hop Step Sheep"));
        assert_eq!(
            album.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://open.spotify.com/album/0LCmjkgN0CqcPtLuNHUBma".to_string()
            ])
        );

        Ok(())
    }
}

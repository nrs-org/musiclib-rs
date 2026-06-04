use std::{collections::HashSet, sync::Arc};

use crate::providers::{
    backends::discogs::{
        SOURCE,
        canonicalize::{match_track_url, track_url},
        client::DiscogsClient,
        release::{
            ReleaseResponse, artist_child_refs, get_release_raw, parse_duration_ms,
            parse_track_position,
        },
    },
    types::{Alias, CachedChildSource, EntityResult, EntrySpecificData, Error, TrackPosition},
};

pub async fn get_track(client: &DiscogsClient, url: &str) -> Result<EntityResult<()>, Error> {
    let (release_id, target_pos) = match_track_url(url).expect("Invalid Discogs track URL");

    get_release_raw::<ReleaseResponse, _, _, Error, _>(
        client,
        &format!("https://www.discogs.com/release/{release_id}"),
        |r, id| {
            let track = if target_pos.synthetic {
                // Index-based fallback for tracks whose position string couldn't be parsed.
                let index = (target_pos.track_no - 1) as usize;
                r.tracklist
                    .iter()
                    .filter(|t| t.track_type != "heading")
                    .nth(index)
            } else {
                r.tracklist
                    .iter()
                    .filter(|t| t.track_type != "heading")
                    .find(|t| {
                        parse_track_position(&t.position)
                            .is_some_and(|p| positions_match(&p, &target_pos))
                    })
            };

            let result = match track {
                None => Err(Error::InvalidUrl(url.to_string())),
                Some(t) => {
                    let position = parse_track_position(&t.position);
                    let canonical = track_url(id, position.as_ref().unwrap_or(&target_pos));

                    let duration_ms = t
                        .duration
                        .as_deref()
                        .filter(|d| !d.is_empty())
                        .and_then(parse_duration_ms);

                    let mut artist_refs = if !t.artists.is_empty() {
                        artist_child_refs(&t.artists)
                    } else {
                        artist_child_refs(&r.artists)
                    };
                    artist_refs.extend(artist_child_refs(&t.extraartists));

                    Ok(EntityResult {
                        release_date: None,
                        sources: [(SOURCE.into(), HashSet::from([canonical]))].into(),
                        extra: serde_json::Value::Null,
                        specific_data: EntrySpecificData::Track {
                            duration_ms,
                            positions: position
                                .map(|p| [(SOURCE.into(), p)].into())
                                .unwrap_or_default(),
                        },
                        children: vec![Arc::new(CachedChildSource::from_children(artist_refs))],
                        aliases: vec![Alias {
                            name: t.title.clone(),
                            source: SOURCE.into(),
                            primary: true,
                            ..Default::default()
                        }],
                    })
                }
            };
            async move { result }
        },
    )
    .await
}

/// Two positions match if their track_no is equal and their disc_no is equal
/// (treating None and Some(1) as the same for single-disc releases).
fn positions_match(a: &TrackPosition, b: &TrackPosition) -> bool {
    if a.track_no != b.track_no {
        return false;
    }
    let disc_a = a.disc_no.unwrap_or(1);
    let disc_b = b.disc_no.unwrap_or(1);
    disc_a == disc_b
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::{collections::HashSet, sync::Arc};

    use http::Method;

    use crate::{
        providers::{
            backends::discogs::{
                SOURCE, client::DiscogsClient, release::ReleaseResponse, track::get_track,
            },
            types::{ChildSource, EntrySpecificData},
        },
        test_utils::MockHttpClient,
    };

    fn release_api_url(id: &str) -> String {
        DiscogsClient::build_url(&format!("releases/{id}"), &[])
    }

    #[tokio::test]
    async fn test_get_track_beautiful_circle() -> anyhow::Result<()> {
        let release_id = "34963553";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ReleaseResponse>(
            Method::GET,
            &release_api_url(release_id),
            include_str!("./release_hop_step_sheep.json"),
        );

        let client = DiscogsClient::new_with_client(Arc::new(http_client), None)?;
        let track = get_track(
            &client,
            &format!("https://www.discogs.com/release/{release_id}?track=1"),
        )
        .await?;

        // Alias
        assert_eq!(track.aliases.len(), 1);
        assert_eq!(track.aliases[0].name, "Beautiful Circle");
        assert!(track.aliases[0].primary);

        // Source: canonical pseudo-URL
        assert_eq!(
            track.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!(
                "https://www.discogs.com/release/{release_id}?track=1"
            )])
        );

        // Duration: "3:49" = (3*60 + 49) * 1000 = 229_000 ms
        assert!(matches!(
            track.specific_data,
            EntrySpecificData::Track {
                duration_ms: Some(229_000),
                ..
            }
        ));

        // Position
        let positions = match &track.specific_data {
            EntrySpecificData::Track { positions, .. } => positions,
            _ => panic!("expected Track"),
        };
        let pos = positions.get(SOURCE).expect("expected discogs position");
        assert_eq!(pos.track_no, 1);
        assert_eq!(pos.disc_no, None);

        // Artists: release-level main artist + extraartists (arranger, composer, lyricist)
        assert_eq!(track.children.len(), 1);
        let mut artists = track.children[0].cursor();

        // Main artist (falls back to release artists)
        let (main, _) = child_next(&mut artists)
            .await?
            .expect("expected main artist");
        assert_eq!(main.name.as_deref(), Some("角巻わため"));
        assert_eq!(main.contributions[0].role, "listed_artist");

        // extraartists: Junichi Satou x2 (Arranged By, Composed By), Hideki Hayashi (Lyrics By)
        let (ea1, _) = child_next(&mut artists)
            .await?
            .expect("expected extraartist 1");
        assert_eq!(ea1.name.as_deref(), Some("Junichi Satou"));
        assert_eq!(ea1.contributions[0].role, "Arranged By");

        let (ea2, _) = child_next(&mut artists)
            .await?
            .expect("expected extraartist 2");
        assert_eq!(ea2.name.as_deref(), Some("Junichi Satou"));
        assert_eq!(ea2.contributions[0].role, "Composed By");

        let (ea3, _) = child_next(&mut artists)
            .await?
            .expect("expected extraartist 3");
        assert_eq!(ea3.name.as_deref(), Some("Hideki hayashi"));
        assert_eq!(ea3.contributions[0].role, "Lyrics By");

        assert!(child_next(&mut artists).await?.is_none());

        Ok(())
    }
}

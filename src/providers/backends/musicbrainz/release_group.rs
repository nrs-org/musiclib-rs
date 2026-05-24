use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        artist_credit::{ArtistCredit, artist_credit_child_refs},
        canonicalize::{match_release_group_url, release_group_url, release_url},
        client::MusicBrainzClient,
        types::EXTERNAL_TYPE_RELEASE,
        url::{UrlResource, url_source_key},
    },
    types::{
        Alias, CachedChildSource, ChildRef, EntityResult, EntrySpecificData, EntryType, Error,
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseGroupResponse {
    pub id: String,
    pub title: String,
    #[serde(rename = "primary-type")]
    pub primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    pub secondary_types: Vec<String>,
    #[serde(rename = "first-release-date")]
    pub first_release_date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    pub releases: Vec<ReleaseGroupRelease>,
    #[serde(default)]
    pub relations: Vec<ReleaseGroupRelation>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseGroupRelation {
    #[serde(rename = "target-type")]
    pub target_type: String,
    pub url: Option<UrlResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseGroupRelease {
    pub id: String,
    pub title: Option<String>,
    pub date: Option<String>,
    pub status: Option<String>,
    pub country: Option<String>,
}

// --- Constants ---

pub(crate) const PARTS: &str = "artists+releases+url-rels";

// --- Helpers ---

fn parse_release_date(date_str: &str) -> Option<String> {
    Some(match date_str.len() {
        4 => format!("{date_str}-XX-XX XX:XX:XX"),
        7 => format!("{date_str}-XX XX:XX:XX"),
        10 => format!("{date_str} XX:XX:XX"),
        _ => return None,
    })
}

// --- Public API ---

pub async fn get_release_group_raw<T, F, R, E, FR>(
    client: &MusicBrainzClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let mbid = match_release_group_url(url).expect("Invalid MusicBrainz release-group URL");
    client
        .get::<T, _, _, _, _>(
            &format!("release-group/{mbid}"),
            &[("inc", PARTS)],
            |value| callback(value, mbid),
        )
        .await
}

pub async fn get_release_group(
    client: &MusicBrainzClient,
    url: &str,
) -> Result<EntityResult<()>, Error> {
    get_release_group_raw::<ReleaseGroupResponse, _, _, Error, _>(client, url, |rg, mbid| {
        let url = release_group_url(mbid);

        let release_date = rg
            .first_release_date
            .as_deref()
            .and_then(parse_release_date);

        let release_refs: Vec<ChildRef> = rg
            .releases
            .iter()
            .map(|rel| ChildRef {
                entry_type: EntryType::Release,
                external_type: EXTERNAL_TYPE_RELEASE.into(),
                sources: [(SOURCE.into(), HashSet::from([release_url(&rel.id)]))].into(),
                name: rel.title.clone(),
                ..Default::default()
            })
            .collect();

        let artist_refs = artist_credit_child_refs(&rg.artist_credit);

        let aliases = vec![Alias {
            name: rg.title.clone(),
            source: SOURCE.into(),
            primary: true,
            ..Default::default()
        }];

        let mut sources: crate::providers::types::ExternalSources =
            [(SOURCE.into(), HashSet::from([url]))].into();
        for rel in rg.relations.iter().filter(|r| r.target_type == "url") {
            if let Some(u) = &rel.url {
                let key = url_source_key(&u.resource);
                sources
                    .0
                    .entry(key.into())
                    .or_default()
                    .insert(u.resource.clone());
            }
        }

        let result = Ok(EntityResult {
            release_date,
            sources,
            extra: rg.extra.clone(),
            specific_data: EntrySpecificData::ReleaseGroup {
                primary_type: rg.primary_type.clone(),
            },
            children: vec![
                Arc::new(CachedChildSource::from_children(artist_refs)),
                Arc::new(CachedChildSource::from_children(release_refs)),
            ],
            aliases,
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
            backends::musicbrainz::{
                SOURCE,
                client::MusicBrainzClient,
                release_group::{PARTS, ReleaseGroupResponse, get_release_group},
                types::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_RELEASE},
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn release_group_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("release-group/{mbid}"), &[("inc", PARTS)])
    }

    #[tokio::test]
    async fn test_get_release_group() -> anyhow::Result<()> {
        let mbid = "7cd8d693-ec09-4b33-ad93-f1137ff77e81";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ReleaseGroupResponse>(
            Method::GET,
            &release_group_api_url(mbid),
            include_str!("./release_group_watame_no_uta.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rg = get_release_group(
            &client,
            &format!("https://musicbrainz.org/release-group/{mbid}"),
        )
        .await?;

        // first-release-date
        assert!(
            rg.release_date
                .as_deref()
                .is_some_and(|d| d.starts_with("2022"))
        );

        // Sources
        assert_eq!(
            rg.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!("https://musicbrainz.org/release-group/{mbid}")])
        );

        // Specific data
        assert!(matches!(
            rg.specific_data,
            EntrySpecificData::ReleaseGroup { primary_type: Some(ref t) } if t == "EP"
        ));

        // Alias
        assert_eq!(rg.aliases.len(), 1);
        assert_eq!(rg.aliases[0].name, "わためのうた vol.2.5");

        // children[0] = artists, children[1] = releases
        assert_eq!(rg.children.len(), 2);

        // Artist child: 角巻わため
        let mut artists_cursor = rg.children[0].cursor();
        let (artist, _) = child_next(&mut artists_cursor)
            .await?
            .expect("expected artist");
        assert_eq!(artist.entry_type, EntryType::Artist);
        assert_eq!(artist.external_type.as_ref(), EXTERNAL_TYPE_ARTIST);
        assert_eq!(artist.name.as_deref(), Some("角巻わため"));

        // Release children: 3 releases
        let mut releases_cursor = rg.children[1].cursor();
        let mut releases = Vec::new();
        while let Some((r, _)) = child_next(&mut releases_cursor).await? {
            releases.push(r);
        }
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0].entry_type, EntryType::Release);
        assert_eq!(releases[0].external_type.as_ref(), EXTERNAL_TYPE_RELEASE);
        assert_eq!(
            releases[0].sources.get(SOURCE).unwrap(),
            &HashSet::from([
                "https://musicbrainz.org/release/7b2484b3-4fa8-4b07-a100-88fd7cdc2071".to_string()
            ])
        );
        assert_eq!(releases[0].name.as_deref(), Some("わためのうた vol.2.5"));

        Ok(())
    }
}

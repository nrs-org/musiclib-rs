//! `GET /api/entries/{id}/cover` — a redirect to the entry's picture
//! (docs/plan-images.md). The server only picks the URL; the browser (or the
//! OS media controls) loads the image straight from the provider.
//!
//! Every consumer shows it in a square box, so a square picture comes
//! first: a track shows its release's album art rather than its own
//! video's 16:9 thumbnail. Resolved over the entry's soft-identity
//! component, in this order:
//!
//! 1. its own stored images (`pair_image`), square ones: from a pair's own
//!    fetch before one from a parent's listing, then by provider
//!    (`PROVIDER_ORDER`);
//! 2. an inherited square one: a track takes a release's that lists it
//!    (Spotify no longer returns album art with a track, and Discogs and
//!    MusicBrainz tracks have none), a release group one of its releases',
//!    a release its release group's;
//! 3. steps 1 then 2 again, any shape;
//! 4. a YouTube video's thumbnail, derived from its id.
//!
//! Nothing else: a redirect to a broken image is worse than the 404, which
//! the frontend shows as a placeholder.

use std::collections::HashSet;

use bytes::Bytes;
use hyper::StatusCode;
use musiclib_rs::musicdb::{MusicDb, PairImageRow};

use crate::{
    entries::component_members,
    respond::{self, HyperResponse},
};

type Pair = (String, String);

/// Providers whose pictures are preferred, best first. Unlisted ones come
/// after, in name order.
const PROVIDER_ORDER: &[&str] = &[
    "spotify",
    "discogs",
    "youtube",
    "nicovideo",
    "soundcloud",
    "musicbrainz",
];

/// How long a browser may reuse an answer. A miss is retried sooner, since
/// an import or backfill can fill it.
const FOUND_MAX_AGE: u32 = 24 * 60 * 60;
const MISSING_MAX_AGE: u32 = 10 * 60;

/// Where `source` falls in `PROVIDER_ORDER`, unlisted ones last.
pub(crate) fn provider_rank(source: &str) -> usize {
    PROVIDER_ORDER
        .iter()
        .position(|p| *p == source)
        .unwrap_or(PROVIDER_ORDER.len())
}

/// Whether `row` is square enough to fill a square box. Discogs scans run a
/// little off square; a 16:9 thumbnail is 1.78. Unknown dimensions
/// (Niconico thumbnails) don't count.
fn is_square(row: &PairImageRow) -> bool {
    match (row.width, row.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let (w, h) = (f64::from(w), f64::from(h));
            w.max(h) / w.min(h) <= 1.25
        }
        _ => false,
    }
}

fn best<'a>(rows: impl IntoIterator<Item = &'a PairImageRow>) -> Option<String> {
    let rank = provider_rank;
    rows.into_iter()
        .min_by(|a, b| {
            (!a.fetched, rank(&a.source), &a.source, &a.identifier).cmp(&(
                !b.fetched,
                rank(&b.source),
                &b.source,
                &b.identifier,
            ))
        })
        .map(|r| r.url.clone())
}

/// The best square one of `rows`.
fn best_square(rows: &[PairImageRow]) -> Option<String> {
    best(rows.iter().filter(|r| is_square(r)))
}

/// Every pair of `ids` and of their soft-identity components.
async fn component_pairs(db: &MusicDb, ids: &[i64]) -> anyhow::Result<Vec<Pair>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (identity, _) = db.display_projections_around(ids).await?;
    let members: HashSet<i64> = ids
        .iter()
        .flat_map(|&id| {
            let canonical = identity.component_of(id).unwrap_or(id);
            identity
                .members_by_component
                .get(&canonical)
                .cloned()
                .unwrap_or_else(|| vec![id])
        })
        .collect();
    Ok(db
        .pairs_by_entry_ids(&members.into_iter().collect::<Vec<_>>())
        .await?)
}

/// The entries of `pairs` whose type is `entry_type`.
async fn entries_of_type(
    db: &MusicDb,
    pairs: &[Pair],
    entry_type: &str,
) -> anyhow::Result<Vec<i64>> {
    let ids: Vec<i64> = db
        .source_rows_for_pairs(pairs)
        .await?
        .into_iter()
        .map(|s| s.entry_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    Ok(db
        .entry_rows_by_ids(&ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == entry_type)
        .map(|e| e.id)
        .collect())
}

/// The URL `entry_id`'s cover redirects to, if any.
pub async fn resolve(db: &MusicDb, entry_id: i64) -> anyhow::Result<Option<String>> {
    let members = component_members(db, entry_id).await?;
    let pairs = db.pairs_by_entry_ids(&members).await?;
    if pairs.is_empty() {
        return Ok(None);
    }
    let own = db.pair_image_rows(&pairs).await?;
    if let Some(url) = best_square(&own) {
        return Ok(Some(url));
    }

    let entry_type = db
        .entry_rows_by_ids(&[entry_id])
        .await?
        .into_iter()
        .next()
        .map(|e| e.entry_type);
    let parents = || async {
        let parent_pairs: Vec<Pair> = db
            .child_rows_for_child_pairs(&pairs)
            .await?
            .into_iter()
            .map(|c| (c.parent_source, c.parent_identifier))
            .collect();
        anyhow::Ok(parent_pairs)
    };
    let children = || async {
        let child_pairs: Vec<Pair> = db
            .child_rows_for_parent_pairs(&pairs)
            .await?
            .into_iter()
            .map(|c| (c.child_source, c.child_identifier))
            .collect();
        anyhow::Ok(child_pairs)
    };
    let related = match entry_type.as_deref() {
        Some("track") => entries_of_type(db, &parents().await?, "release").await?,
        Some("release") => entries_of_type(db, &parents().await?, "release_group").await?,
        Some("release_group") => entries_of_type(db, &children().await?, "release").await?,
        _ => Vec::new(),
    };
    let inherited = db
        .pair_image_rows(&component_pairs(db, &related).await?)
        .await?;
    if let Some(url) = best_square(&inherited)
        .or_else(|| best(&own))
        .or_else(|| best(&inherited))
    {
        return Ok(Some(url));
    }

    let mut videos: Vec<&str> = pairs
        .iter()
        .filter(|(source, _)| source == "youtube")
        .filter_map(|(_, identifier)| identifier.strip_prefix("https://youtu.be/"))
        .collect();
    videos.sort_unstable();
    Ok(videos
        .first()
        .map(|id| format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")))
}

/// `GET /api/entries/{id}/cover`.
pub async fn cover(db: &MusicDb, entry_id: i64) -> HyperResponse {
    match resolve(db, entry_id).await {
        Ok(Some(url)) => hyper::Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", url)
            .header("Cache-Control", format!("max-age={FOUND_MAX_AGE}"))
            .body(respond::full(Bytes::new()))
            .expect("response builder"),
        Ok(None) => hyper::Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Cache-Control", format!("max-age={MISSING_MAX_AGE}"))
            .body(respond::full(Bytes::new()))
            .expect("response builder"),
        Err(e) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use musiclib_rs::musicdb::PairImageRow;

    use super::{best, best_square};

    fn row(source: &str, url: &str, fetched: bool) -> PairImageRow {
        sized(source, url, fetched, (300, 300))
    }

    fn sized(source: &str, url: &str, fetched: bool, (w, h): (i32, i32)) -> PairImageRow {
        PairImageRow {
            source: source.to_owned(),
            identifier: url.to_owned(),
            url: url.to_owned(),
            width: Some(w),
            height: Some(h),
            fetched,
        }
    }

    #[test]
    fn fetched_beats_listing_then_provider_order() {
        let rows = vec![
            row("youtube", "yt", true),
            row("spotify", "sp-listed", false),
            row("musicbrainz", "caa", true),
            row("spotify", "sp", true),
        ];
        assert_eq!(best(&rows).as_deref(), Some("sp"));
        assert_eq!(
            best(&[row("spotify", "sp-listed", false)]).as_deref(),
            Some("sp-listed")
        );
        assert_eq!(best(&[]), None);
    }

    #[test]
    fn square_skips_video_thumbnails() {
        let rows = vec![
            sized("youtube", "thumb", true, (320, 180)),
            sized("discogs", "scan", true, (600, 640)),
            PairImageRow {
                width: None,
                height: None,
                ..row("nicovideo", "nico", true)
            },
        ];
        assert_eq!(best_square(&rows).as_deref(), Some("scan"));
        // Only a thumbnail: none square, so the fallback takes it as it is.
        assert_eq!(best_square(&rows[..1]), None);
        assert_eq!(best(&rows[..1]).as_deref(), Some("thumb"));
    }
}

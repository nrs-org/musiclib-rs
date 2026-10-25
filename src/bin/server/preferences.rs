//! Preferred editions and sources: which edition of a track, and which of
//! an edition's sources, the player plays when the choice is left to it.
//!
//! Both are weights the user sets (0 = not preferred). A queued item draws
//! once, when it's queued (`resolve`), in proportion to the weights of
//! whatever is preferred — so several preferred editions or sources take
//! turns at random, and every device plays the same draw. Nothing
//! preferred leaves the item as queued: the group's default edition, from
//! whichever source the active device's provider preference picks.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use hyper::StatusCode;
use musiclib_rs::musicdb::{EditionProjection, MusicDb, SoftIdentityProjection};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::{
    entries::{component_members, entry_summaries_with},
    playback::QueueItem,
    player::embed_for,
    respond::{self, HyperResponse},
    state::AppState,
};

/// Weights above this are clamped: a hundredfold preference is already
/// "always", short of the others' weight dropping to 0.
pub const MAX_WEIGHT: u32 = 99;

/// One of `options`, drawn in proportion to its weight; `None` when no
/// option has any weight.
pub fn pick_weighted<T>(options: &[(T, u32)]) -> Option<&T> {
    let total: u64 = options.iter().map(|(_, w)| u64::from(*w)).sum();
    if total == 0 {
        return None;
    }
    let mut roll = fastrand::u64(..total);
    for (option, weight) in options {
        let weight = u64::from(*weight);
        if roll < weight {
            return Some(option);
        }
        roll -= weight;
    }
    unreachable!("roll < total, the sum of the weights")
}

/// Draws a preferred edition (if `pick_editions`) and then a preferred
/// source for each of `items`, in place. Only an item queued as its edition
/// group's default edition, outside an album, swaps edition: an album's
/// tracklist plays the album's own recordings, and an edition picked by
/// hand plays as picked. Preferences are a nicety, so a failed lookup just
/// leaves the items as queued.
pub async fn resolve(db: &MusicDb, items: &mut [QueueItem], pick_editions: bool) {
    if items.is_empty() {
        return;
    }
    if let Err(e) = try_resolve(db, items, pick_editions).await {
        warn!("couldn't apply play preferences: {e:#}");
    }
}

async fn try_resolve(
    db: &MusicDb,
    items: &mut [QueueItem],
    pick_editions: bool,
) -> anyhow::Result<()> {
    let ids: Vec<i64> = items.iter().map(|it| it.entry_id).collect();
    let (identity, editions) = db.display_projections_around(&ids).await?;
    if pick_editions {
        pick_editions_for(db, items, &identity, &editions).await?;
    }
    pick_sources_for(db, items, &identity).await
}

fn canonical(identity: &SoftIdentityProjection, id: i64) -> i64 {
    identity.component_of(id).unwrap_or(id)
}

/// `canonical`'s soft-identity component: the entries whose sources and
/// preferences all count as its own.
fn component(identity: &SoftIdentityProjection, canonical: i64) -> Vec<i64> {
    identity
        .members_by_component
        .get(&canonical)
        .cloned()
        .unwrap_or_else(|| vec![canonical])
}

/// Each of `editions`' (identity-canonical ids) preference weight: any of
/// its component members' (`set_preferred_edition` keeps one per
/// component).
async fn edition_weights_with(
    db: &MusicDb,
    identity: &SoftIdentityProjection,
    editions: &[i64],
) -> anyhow::Result<HashMap<i64, u32>> {
    let components: Vec<(i64, Vec<i64>)> = editions
        .iter()
        .map(|&e| (e, component(identity, e)))
        .collect();
    let all: Vec<i64> = components
        .iter()
        .flat_map(|(_, m)| m.iter().copied())
        .collect();
    let weights = db.preferred_edition_weights(&all).await?;
    Ok(components
        .into_iter()
        .filter_map(|(e, members)| {
            let w = members.iter().filter_map(|m| weights.get(m)).max()?;
            Some((e, *w))
        })
        .collect())
}

/// The preference weight of each of `editions` (identity-canonical ids of
/// one entry page's editions); unpreferred ones are absent.
pub async fn edition_weights(db: &MusicDb, editions: &[i64]) -> anyhow::Result<HashMap<i64, u32>> {
    let (identity, _) = db.display_projections_around(editions).await?;
    edition_weights_with(db, &identity, editions).await
}

async fn pick_editions_for(
    db: &MusicDb,
    items: &mut [QueueItem],
    identity: &SoftIdentityProjection,
    editions: &EditionProjection,
) -> anyhow::Result<()> {
    // Item index -> its group's editions, for every item queued as the
    // default edition of a group that has others.
    let mut choices: Vec<(usize, Vec<i64>)> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let c = canonical(identity, item.entry_id);
        if item.release_id.is_some() || editions.default_member_of(c) != c {
            continue;
        }
        let members = editions.members_of(c);
        if members.len() > 1 {
            choices.push((idx, members));
        }
    }
    if choices.is_empty() {
        return Ok(());
    }
    let all: Vec<i64> = choices
        .iter()
        .flat_map(|(_, eds)| eds.iter().copied())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut weights = edition_weights_with(db, identity, &all).await?;
    // An edition nothing here can play would only be skipped.
    let preferred: Vec<i64> = weights.keys().copied().collect();
    let playable = playable_editions(db, identity, &preferred).await?;
    weights.retain(|e, _| playable.contains(e));
    let mut swapped = Vec::new();
    for (idx, eds) in choices {
        let weighted: Vec<(i64, u32)> = eds
            .iter()
            .map(|&e| (e, weights.get(&e).copied().unwrap_or(0)))
            .collect();
        let Some(&pick) = pick_weighted(&weighted) else {
            continue;
        };
        if pick != canonical(identity, items[idx].entry_id) {
            items[idx].entry_id = pick;
            items[idx].title = None;
            swapped.push(pick);
        }
    }
    if swapped.is_empty() {
        return Ok(());
    }
    // A swapped-in edition goes by its own title, not the original's.
    let summaries = entry_summaries_with(db, &swapped, identity, editions).await?;
    for item in items.iter_mut().filter(|it| it.title.is_none()) {
        if let Some(summary) = summaries.get(&item.entry_id) {
            item.title = summary.title.clone();
        }
    }
    Ok(())
}

/// Which of `editions` (identity-canonical ids) have a source the player
/// can embed, across their components.
async fn playable_editions(
    db: &MusicDb,
    identity: &SoftIdentityProjection,
    editions: &[i64],
) -> anyhow::Result<HashSet<i64>> {
    let all: Vec<i64> = editions
        .iter()
        .flat_map(|&e| component(identity, e))
        .collect();
    let rows = db.source_rows_by_entry_ids(&all).await?;
    Ok(rows
        .iter()
        .filter(|row| embed_for(&row.source, &row.identifier).is_some())
        .map(|row| canonical(identity, row.entry_id))
        .collect())
}

async fn pick_sources_for(
    db: &MusicDb,
    items: &mut [QueueItem],
    identity: &SoftIdentityProjection,
) -> anyhow::Result<()> {
    // Each item's component (whose sources `playable-sources` offers).
    let components: Vec<Vec<i64>> = items
        .iter()
        .map(|it| component(identity, canonical(identity, it.entry_id)))
        .collect();
    let all: Vec<i64> = components
        .iter()
        .flatten()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let rows = db.source_rows_by_entry_ids(&all).await?;
    let mut playable_by_entry: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    for row in rows {
        if embed_for(&row.source, &row.identifier).is_some() {
            playable_by_entry
                .entry(row.entry_id)
                .or_default()
                .push((row.source, row.identifier));
        }
    }
    let pairs: Vec<(String, String)> = playable_by_entry.values().flatten().cloned().collect();
    let weights = db.preferred_source_weights(&pairs).await?;
    if weights.is_empty() {
        return Ok(());
    }
    for (item, members) in items.iter_mut().zip(&components) {
        let weighted: Vec<(&str, u32)> = members
            .iter()
            .filter_map(|m| playable_by_entry.get(m))
            .flatten()
            .filter_map(|pair| Some((pair.1.as_str(), *weights.get(pair)?)))
            .collect();
        if let Some(identifier) = pick_weighted(&weighted) {
            item.source = Some((*identifier).to_owned());
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct EditionPreferenceRequest {
    weight: u32,
}

/// `POST /api/entries/{id}/preferred-edition` — `{weight}`: how strongly
/// to prefer edition `id` when its group is queued (0 clears it).
pub async fn set_edition(state: &Arc<AppState>, id: &str, body: &[u8]) -> HyperResponse {
    let Ok(entry_id) = id.parse::<i64>() else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "id must be numeric",
        );
    };
    let req: EditionPreferenceRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    let weight = req.weight.min(MAX_WEIGHT);
    let result = async {
        let members = component_members(&state.db, entry_id).await?;
        state
            .db
            .set_preferred_edition(entry_id, &members, weight)
            .await?;
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => respond::json(
            StatusCode::OK,
            json!({ "entry_id": entry_id, "weight": weight }),
        ),
        Err(e) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct SourcePreferenceRequest {
    source: String,
    identifier: String,
    weight: u32,
}

/// `POST /api/preferred-source` — `{source, identifier, weight}`: how
/// strongly to prefer playing from that source (0 clears it).
pub async fn set_source(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: SourcePreferenceRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    if embed_for(&req.source, &req.identifier).is_none() {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "not a playable source",
        );
    }
    let weight = req.weight.min(MAX_WEIGHT);
    match state
        .db
        .set_preferred_source(&req.source, &req.identifier, weight)
        .await
    {
        Ok(()) => respond::json(
            StatusCode::OK,
            json!({ "source": req.source, "identifier": req.identifier, "weight": weight }),
        ),
        Err(e) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use musiclib_rs::providers::types::EntrySpecificData;

    use super::*;

    #[test]
    fn nothing_weighted_picks_nothing() {
        assert_eq!(pick_weighted::<i64>(&[]), None);
        assert_eq!(pick_weighted(&[(1, 0), (2, 0)]), None);
    }

    #[test]
    fn unweighted_options_are_never_picked() {
        for _ in 0..200 {
            assert_eq!(pick_weighted(&[(1, 0), (2, 3), (3, 0)]), Some(&2));
        }
    }

    #[test]
    fn picks_follow_the_weights() {
        fastrand::seed(7);
        let options = [("a", 1), ("b", 3)];
        let mut b = 0;
        let draws = 4000;
        for _ in 0..draws {
            if pick_weighted(&options) == Some(&"b") {
                b += 1;
            }
        }
        // Expect 3000; a 4-sigma band is about ±110.
        assert!((2850..3150).contains(&b), "b drawn {b} times of {draws}");
    }

    async fn track(db: &MusicDb, video_ids: &[&str]) -> i64 {
        let entry_id = db.insert_entry(None).await.unwrap();
        for id in video_ids {
            db.upsert_pair(
                "youtube",
                &format!("https://youtu.be/{id}"),
                entry_id,
                None,
                &EntrySpecificData::Track {
                    duration_ms: vec![],
                    positions: Default::default(),
                    songs: vec![],
                },
                None,
            )
            .await
            .unwrap();
        }
        entry_id
    }

    fn queued(entry_id: i64, release_id: Option<i64>) -> QueueItem {
        QueueItem {
            entry_id,
            title: Some("queued".into()),
            release_id,
            source: None,
        }
    }

    #[tokio::test]
    async fn queued_items_play_the_preferred_edition_and_source() {
        let db = MusicDb::new("sqlite::memory:").await.unwrap();
        let original = track(&db, &["orig1", "orig2"]).await;
        let cover = track(&db, &["cover1"]).await;
        db.set_edition_derivation(original, cover, Some("cover"))
            .await
            .unwrap();

        // Nothing preferred: played as queued.
        let mut items = [queued(original, None)];
        resolve(&db, &mut items, true).await;
        assert_eq!(items[0], queued(original, None));

        // The cover preferred: queuing the original plays the cover.
        db.set_preferred_edition(cover, &[cover], 1).await.unwrap();
        let mut items = [queued(original, None)];
        resolve(&db, &mut items, true).await;
        assert_eq!(items[0].entry_id, cover);
        assert_eq!(items[0].source, None);

        // ...from its preferred source.
        db.set_preferred_source("youtube", "https://youtu.be/cover1", 2)
            .await
            .unwrap();
        let mut items = [queued(original, None)];
        resolve(&db, &mut items, true).await;
        assert_eq!(items[0].source.as_deref(), Some("https://youtu.be/cover1"));

        // An album's track, and an edition switched to by hand, stay put;
        // the latter still plays from its preferred source.
        let mut items = [queued(original, Some(99))];
        resolve(&db, &mut items, true).await;
        assert_eq!(items[0].entry_id, original);
        let mut items = [queued(original, None)];
        resolve(&db, &mut items, false).await;
        assert_eq!(items[0].entry_id, original);
        db.set_preferred_source("youtube", "https://youtu.be/orig2", 1)
            .await
            .unwrap();
        resolve(&db, &mut items, false).await;
        assert_eq!(items[0].source.as_deref(), Some("https://youtu.be/orig2"));

        // Weight 0 clears a preference.
        db.set_preferred_edition(cover, &[cover], 0).await.unwrap();
        let mut items = [queued(original, None)];
        resolve(&db, &mut items, true).await;
        assert_eq!(items[0].entry_id, original);

        // An edition with nothing playable is never drawn.
        let unplayable = track(&db, &[]).await;
        db.set_edition_derivation(original, unplayable, Some("live"))
            .await
            .unwrap();
        db.set_preferred_edition(unplayable, &[unplayable], 5)
            .await
            .unwrap();
        let mut items = [queued(original, None)];
        resolve(&db, &mut items, true).await;
        assert_eq!(items[0].entry_id, original);
    }
}

//! A Spotify collection (playlist, album, Liked Songs or a single track) seen through what we
//! already have on disk: every track's state, matched by Spotify track id, plus how the judge
//! decided on it.

use std::collections::{HashMap, HashSet};

use actix_web::{HttpResponse, get, post, web};
use diesel::prelude::*;
use diesel::sql_types::{Array, BigInt, Integer, Nullable, Text};
use redis::Commands;
use serde::{Deserialize, Serialize};

use crate::api::{self, JudgementResponse};
use crate::auth::Principal;
use crate::errors::{ApiError, ApiResult};
use crate::spotify_user::{self, API_BASE, Image, SpotifyError};
use crate::state::AppState;
use convert_invert::internals::judge::judge_manager::JUDGE_THRESHOLD;
use convert_invert::internals::worker::worker_manager::{request_id_from_run_id, sanitize_folder_name};

const PAGE_DEFAULT: usize = 100;
const PAGE_MAX: usize = 500;
const SNAPSHOT_TTL_SECS: u64 = 180;
/// Pending-request markers outlive any realistic run; live-worker checks do the real expiry.
pub const PENDING_TTL_SECS: u64 = 86_400;
/// Track ids per collection, kept long enough that the library remembers what you've opened.
const IDS_TTL_SECS: u64 = 30 * 86_400;
const STATUS_MAX_ITEMS: usize = 300;

// ---------------------------------------------------------------------------------------------
// Collection kinds and Spotify payloads

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    Playlist,
    Album,
    Liked,
    Track,
}

impl CollectionKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "playlist" => Some(Self::Playlist),
            "album" => Some(Self::Album),
            "liked" => Some(Self::Liked),
            "track" => Some(Self::Track),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Album => "album",
            Self::Liked => "liked",
            Self::Track => "track",
        }
    }
}

/// Spotify ids are base62; reject anything else before it reaches a URL or a Redis key.
pub fn valid_spotify_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CollectionTrack {
    pub spotify_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub image_url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CollectionMeta {
    pub kind: CollectionKind,
    pub id: String,
    pub name: String,
    pub owner: String,
    pub image_url: Option<String>,
    pub total: usize,
    pub duration_ms: u64,
}

pub struct Resolved {
    pub meta: CollectionMeta,
    pub tracks: Vec<CollectionTrack>,
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct NamedArtist {
    name: String,
}

#[derive(Deserialize)]
struct AlbumRef {
    name: String,
    #[serde(default)]
    images: Vec<Image>,
}

#[derive(Deserialize)]
struct FullTrack {
    id: Option<String>,
    name: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    artists: Vec<NamedArtist>,
    album: Option<AlbumRef>,
    #[serde(default)]
    is_local: bool,
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct PlaylistItem {
    track: Option<FullTrack>,
}

#[derive(Deserialize)]
struct PlaylistOwner {
    id: String,
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct PlaylistMeta {
    name: String,
    owner: PlaylistOwner,
    #[serde(default)]
    images: Option<Vec<Image>>,
    snapshot_id: String,
}

#[derive(Deserialize)]
struct AlbumMeta {
    name: String,
    #[serde(default)]
    artists: Vec<NamedArtist>,
    #[serde(default)]
    images: Vec<Image>,
    tracks: Page<FullTrack>,
}

fn join_artists(artists: &[NamedArtist]) -> String {
    artists.iter().map(|artist| artist.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// Skip removed tracks, podcast episodes and local files: none of them can be searched for.
fn to_track(track: FullTrack, album_fallback: Option<(&str, Option<&str>)>) -> Option<CollectionTrack> {
    if track.is_local || track.kind.as_deref().is_some_and(|kind| kind != "track") {
        return None;
    }
    let spotify_id = track.id?;
    let (album, image_url) = match (&track.album, album_fallback) {
        (Some(album), _) => (album.name.clone(), album.images.first().map(|image| image.url.clone())),
        (None, Some((name, image))) => (name.to_string(), image.map(str::to_string)),
        (None, None) => (String::new(), None),
    };
    Some(CollectionTrack {
        spotify_id,
        title: track.name,
        artist: join_artists(&track.artists),
        album,
        duration_ms: track.duration_ms,
        image_url,
    })
}

async fn pages<T: serde::de::DeserializeOwned>(
    token: &str,
    mut next: Option<String>,
    max_pages: usize,
) -> Result<Vec<T>, SpotifyError> {
    let mut items = Vec::new();
    for _ in 0..max_pages {
        let Some(url) = next.take() else { break };
        let page: Page<T> = spotify_user::spotify_get(token, &url).await?;
        items.extend(page.items);
        next = page.next;
    }
    Ok(items)
}

fn cache_get<T: serde::de::DeserializeOwned>(state: &AppState, key: &str) -> Option<T> {
    let mut con = state.redis_pool.get().ok()?;
    let raw: Option<String> = con.get(key).ok()?;
    raw.and_then(|raw| serde_json::from_str(&raw).ok())
}

fn cache_set<T: Serialize>(state: &AppState, key: &str, value: &T, ttl_secs: u64) {
    if let (Ok(mut con), Ok(raw)) = (state.redis_pool.get(), serde_json::to_string(value)) {
        let _: Result<(), _> = con.set_ex(key, raw, ttl_secs);
    }
}

#[derive(Serialize, Deserialize)]
struct CachedCollection {
    meta: CollectionMeta,
    tracks: Vec<CollectionTrack>,
}

fn finish(kind: CollectionKind, id: &str, name: String, owner: String, image_url: Option<String>, tracks: Vec<CollectionTrack>) -> Resolved {
    let duration_ms = tracks.iter().map(|track| track.duration_ms).sum();
    Resolved {
        meta: CollectionMeta {
            kind,
            id: id.to_string(),
            name,
            owner,
            image_url,
            total: tracks.len(),
            duration_ms,
        },
        tracks,
    }
}

/// The collection's tracks in Spotify order, read with the person's own token (so private
/// playlists and Liked Songs work). Cached in Redis: playlists per `snapshot_id`, albums and
/// tracks for a day, Liked Songs for two minutes per person.
pub async fn resolve(
    state: &AppState,
    user_id: i32,
    owner_name: &str,
    kind: CollectionKind,
    id: &str,
) -> Result<Resolved, SpotifyError> {
    let resolved = resolve_tracks(state, user_id, owner_name, kind, id).await?;
    let ids: Vec<&str> = resolved.tracks.iter().map(|track| track.spotify_id.as_str()).collect();
    cache_set(state, &ids_key(user_id, kind, id), &ids, IDS_TTL_SECS);
    Ok(resolved)
}

/// Where the last known track ids of a collection live, for the library's progress badges.
fn ids_key(user_id: i32, kind: CollectionKind, id: &str) -> String {
    match kind {
        CollectionKind::Liked => format!("collids:liked:{user_id}"),
        _ => format!("collids:{}:{id}", kind.as_str()),
    }
}

async fn resolve_tracks(
    state: &AppState,
    user_id: i32,
    owner_name: &str,
    kind: CollectionKind,
    id: &str,
) -> Result<Resolved, SpotifyError> {
    if kind != CollectionKind::Liked && !valid_spotify_id(id) {
        return Err(SpotifyError::Upstream(format!("'{id}' is not a Spotify id")));
    }
    let token = spotify_user::access_token_for(state, user_id).await?;
    let from_cache = |key: &str| cache_get::<CachedCollection>(state, key).map(|c| Resolved { meta: c.meta, tracks: c.tracks });
    let store = |key: &str, resolved: &Resolved, ttl: u64| {
        cache_set(state, key, &CachedCollection { meta: resolved.meta.clone(), tracks: resolved.tracks.clone() }, ttl)
    };

    match kind {
        CollectionKind::Playlist => {
            let meta: PlaylistMeta = spotify_user::spotify_get(
                &token,
                &format!("{API_BASE}/playlists/{id}?fields=name,owner(id,display_name),images,snapshot_id"),
            )
            .await?;
            let key = format!("coll:playlist:{id}:{}", meta.snapshot_id);
            if let Some(cached) = from_cache(&key) {
                return Ok(cached);
            }
            let items: Vec<PlaylistItem> = pages(
                &token,
                Some(format!(
                    "{API_BASE}/playlists/{id}/tracks?limit=100&fields=next,items(track(id,name,duration_ms,is_local,type,artists(name),album(name,images)))"
                )),
                100,
            )
            .await?;
            let tracks = items.into_iter().filter_map(|item| item.track.and_then(|t| to_track(t, None))).collect();
            let resolved = finish(
                kind,
                id,
                meta.name,
                meta.owner.display_name.unwrap_or(meta.owner.id),
                meta.images.and_then(|images| images.into_iter().next()).map(|image| image.url),
                tracks,
            );
            store(&key, &resolved, 3_600);
            Ok(resolved)
        }
        CollectionKind::Album => {
            let key = format!("coll:album:{id}");
            if let Some(cached) = from_cache(&key) {
                return Ok(cached);
            }
            let album: AlbumMeta = spotify_user::spotify_get(&token, &format!("{API_BASE}/albums/{id}")).await?;
            let image = album.images.first().map(|image| image.url.clone());
            let mut raw = album.tracks.items;
            raw.extend(pages::<FullTrack>(&token, album.tracks.next, 20).await?);
            let tracks = raw
                .into_iter()
                .filter_map(|t| to_track(t, Some((&album.name, image.as_deref()))))
                .collect();
            let resolved = finish(kind, id, album.name.clone(), join_artists(&album.artists), image, tracks);
            store(&key, &resolved, 86_400);
            Ok(resolved)
        }
        CollectionKind::Liked => {
            let key = format!("coll:liked:{user_id}");
            if let Some(cached) = from_cache(&key) {
                return Ok(cached);
            }
            #[derive(Deserialize)]
            struct Saved {
                track: FullTrack,
            }
            let saved: Vec<Saved> = pages(&token, Some(format!("{API_BASE}/me/tracks?limit=50")), 200).await?;
            let tracks = saved.into_iter().filter_map(|s| to_track(s.track, None)).collect();
            let resolved = finish(kind, "liked", "Liked Songs".to_string(), owner_name.to_string(), None, tracks);
            store(&key, &resolved, 120);
            Ok(resolved)
        }
        CollectionKind::Track => {
            let key = format!("coll:track:{id}");
            if let Some(cached) = from_cache(&key) {
                return Ok(cached);
            }
            let track: FullTrack = spotify_user::spotify_get(&token, &format!("{API_BASE}/tracks/{id}")).await?;
            let resolved = match to_track(track, None) {
                Some(track) => finish(kind, id, track.title.clone(), track.artist.clone(), track.image_url.clone(), vec![track]),
                None => return Err(SpotifyError::Upstream("That track can't be downloaded (local file or podcast)".into())),
            };
            store(&key, &resolved, 86_400);
            Ok(resolved)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Track states

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Requester {
    pub id: i32,
    pub name: String,
    pub avatar_url: Option<String>,
}

/// How the judge decided. `chosen` for downloaded tracks, `rejected` for failed ones.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JudgingSummary {
    pub outcome: &'static str,
    pub label: Option<String>,
    pub score: Option<f32>,
    pub filename: Option<String>,
    pub username: Option<String>,
    pub size: Option<i64>,
    pub format: Option<String>,
    pub uncertain: bool,
    pub candidates_checked: i64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrackStatus {
    /// owned | downloading | comparing | searching | queued | failed | missing
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_db_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judging: Option<JudgingSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<Requester>,
}

impl TrackStatus {
    fn missing() -> Self {
        TrackStatus { state: "missing", progress: None, track_db_id: None, judging: None, requested_by: None }
    }

    pub fn in_flight(&self) -> bool {
        matches!(self.state, "downloading" | "comparing" | "searching" | "queued")
    }

    pub fn needs_download(&self) -> bool {
        matches!(self.state, "missing" | "failed")
    }
}

/// Everything known about one track id, before precedence is applied.
#[derive(Default, Clone, Debug)]
pub struct TrackFacts {
    pub track_db_id: Option<i32>,
    pub owned: bool,
    pub rejections: i64,
    pub request_id: Option<i32>,
    pub download_progress: Option<i32>,
    /// Activity marker stage: "searching" | "judging".
    pub activity: Option<String>,
    /// Pending marker from a request: (request id, rejections when it was queued).
    pub pending: Option<(i32, i64)>,
}

/// Precedence from the spec: owned > in flight > failed > missing. `live_requests` are the
/// requests that still have workers running; a pending marker only means "queued" while its
/// request is live and no new rejection has landed since it was queued.
pub fn derive_state(facts: &TrackFacts, live_requests: &HashSet<i32>) -> (&'static str, Option<i32>) {
    if facts.owned {
        return ("owned", None);
    }
    if let Some(progress) = facts.download_progress {
        return ("downloading", Some(progress));
    }
    match facts.activity.as_deref() {
        Some("judging") => return ("comparing", None),
        Some(_) => return ("searching", None),
        None => {}
    }
    if let Some((request_id, rejections_then)) = facts.pending
        && live_requests.contains(&request_id)
        && facts.rejections <= rejections_then
    {
        return ("queued", None);
    }
    if facts.rejections > 0 {
        return ("failed", None);
    }
    ("missing", None)
}

#[derive(QueryableByName)]
struct FactRow {
    #[diesel(sql_type = Integer)]
    id: i32,
    #[diesel(sql_type = Text)]
    track_id: String,
    #[diesel(sql_type = Nullable<Integer>)]
    request_id: Option<i32>,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    owned: bool,
    #[diesel(sql_type = BigInt)]
    rejections: i64,
}

#[derive(QueryableByName)]
struct ChosenRow {
    #[diesel(sql_type = Text)]
    track_id: String,
    #[diesel(sql_type = Text)]
    filename: String,
    #[diesel(sql_type = Text)]
    username: String,
    #[diesel(sql_type = BigInt)]
    size: i64,
    #[diesel(sql_type = Nullable<diesel::sql_types::Float>)]
    score: Option<f32>,
    #[diesel(sql_type = Nullable<Text>)]
    label: Option<String>,
    #[diesel(sql_type = BigInt)]
    candidates: i64,
}

#[derive(QueryableByName)]
struct RejectedRow {
    #[diesel(sql_type = Text)]
    track_id: String,
    #[diesel(sql_type = BigInt)]
    candidates: i64,
    #[diesel(sql_type = Nullable<Text>)]
    top_label: Option<String>,
    #[diesel(sql_type = Nullable<diesel::sql_types::Float>)]
    best_score: Option<f32>,
}

#[derive(QueryableByName)]
struct RequesterRow {
    #[diesel(sql_type = Integer)]
    request_id: i32,
    #[diesel(sql_type = Integer)]
    user_id: i32,
    #[diesel(sql_type = Text)]
    name: String,
    #[diesel(sql_type = Nullable<Text>)]
    avatar_url: Option<String>,
}

pub fn uncertain_below() -> f32 {
    std::env::var("JUDGE_UNCERTAIN_BELOW")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(JUDGE_THRESHOLD)
}

/// Doubtful acceptance: the deciding judge said something other than "exact", or the score is
/// below the threshold.
pub fn is_uncertain(label: Option<&str>, score: Option<f32>, threshold: f32) -> bool {
    label.is_some_and(|label| label != "exact") || score.is_some_and(|score| score < threshold)
}

fn format_of(filename: &str) -> Option<String> {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    base.rsplit_once('.').map(|(_, ext)| ext.to_lowercase()).filter(|ext| !ext.is_empty() && ext.len() <= 5)
}

/// Request ids that still have a live worker.
pub fn live_requests(state: &AppState) -> HashSet<i32> {
    state
        .worker_supervisor
        .status()
        .map(|status| status.workers.iter().filter_map(|worker| request_id_from_run_id(&worker.run_id)).collect())
        .unwrap_or_default()
}

pub fn pending_key(spotify_id: &str) -> String {
    format!("req:pending:{spotify_id}")
}

/// Raw facts for each Spotify id (only ids we have seen get an entry).
pub fn track_facts(state: &AppState, spotify_ids: &[String]) -> ApiResult<HashMap<String, TrackFacts>> {
    let mut facts: HashMap<String, TrackFacts> = HashMap::new();
    if spotify_ids.is_empty() {
        return Ok(facts);
    }
    let mut connection = state.db_pool.get()?;
    let rows = diesel::sql_query(
        r#"
        SELECT si.id, si.track_id, si.request_id,
            EXISTS (
                SELECT 1 FROM downloaded_file df
                WHERE df.track = si.id OR df.filename IN (
                    SELECT dlf.filename FROM downloadable_files dlf
                    JOIN judge_submissions js ON js.query = dlf.id
                    WHERE js.track = si.id
                )
            ) AS owned,
            (SELECT COUNT(*) FROM rejected_track rt JOIN judge_submissions js ON rt.track = js.id WHERE js.track = si.id) AS rejections
        FROM search_items si
        WHERE si.track_id = ANY($1)
        "#,
    )
    .bind::<Array<Text>, _>(spotify_ids)
    .load::<FactRow>(&mut connection)?;

    let mut by_db_id = HashMap::new();
    for row in rows {
        by_db_id.insert(row.id, row.track_id.clone());
        facts.insert(
            row.track_id,
            TrackFacts {
                track_db_id: Some(row.id),
                owned: row.owned,
                rejections: row.rejections,
                request_id: row.request_id,
                ..TrackFacts::default()
            },
        );
    }

    for download in api::collect_active_downloads(&state.redis_pool).unwrap_or_default() {
        if let Some(spotify_id) = download.track_db_id.and_then(|id| by_db_id.get(&id))
            && let Some(entry) = facts.get_mut(spotify_id)
        {
            entry.download_progress = Some(download.progress.clamp(0, 100));
        }
    }
    let wanted: HashSet<&str> = spotify_ids.iter().map(String::as_str).collect();
    for marker in api::activity_markers(&state.redis_pool).unwrap_or_default() {
        if wanted.contains(marker.track_id.as_str()) {
            facts.entry(marker.track_id.clone()).or_default().activity = Some(marker.stage);
        }
    }

    if let Ok(mut con) = state.redis_pool.get() {
        let mut pipe = redis::pipe();
        for id in spotify_ids {
            pipe.hgetall(pending_key(id));
        }
        let pending: Vec<HashMap<String, String>> = pipe.query(&mut con).unwrap_or_default();
        for (id, marker) in spotify_ids.iter().zip(pending) {
            let request = marker.get("request_id").and_then(|v| v.parse::<i32>().ok());
            let rejections = marker.get("rejections").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            if let Some(request) = request {
                facts.entry(id.clone()).or_default().pending = Some((request, rejections));
            }
        }
    }
    Ok(facts)
}

/// Final state for every id in `spotify_ids`, with judging summaries and requesters attached.
pub fn track_statuses(state: &AppState, spotify_ids: &[String]) -> ApiResult<HashMap<String, TrackStatus>> {
    let facts = track_facts(state, spotify_ids)?;
    let live = live_requests(state);
    let mut statuses: HashMap<String, TrackStatus> = HashMap::with_capacity(spotify_ids.len());
    let mut request_for: HashMap<String, i32> = HashMap::new();
    for id in spotify_ids {
        let Some(fact) = facts.get(id) else {
            statuses.insert(id.clone(), TrackStatus::missing());
            continue;
        };
        let (state_name, progress) = derive_state(fact, &live);
        let status = TrackStatus { state: state_name, progress, track_db_id: fact.track_db_id, judging: None, requested_by: None };
        if status.in_flight() {
            let request = if state_name == "queued" { fact.pending.map(|(r, _)| r) } else { fact.request_id.or(fact.pending.map(|(r, _)| r)) };
            if let Some(request) = request {
                request_for.insert(id.clone(), request);
            }
        }
        statuses.insert(id.clone(), status);
    }

    let mut connection = state.db_pool.get()?;
    let owned: Vec<String> = statuses.iter().filter(|(_, s)| s.state == "owned").map(|(id, _)| id.clone()).collect();
    if !owned.is_empty() {
        let threshold = uncertain_below();
        let rows = diesel::sql_query(
            r#"
            SELECT DISTINCT ON (si.track_id)
                si.track_id, dlf.filename, dlf.username, dlf.size, js.score,
                (SELECT j.label FROM judgements j WHERE j.submission = js.id AND j.label IS NOT NULL
                 ORDER BY (j.judge = 'manual') DESC, j.id DESC LIMIT 1) AS label,
                (SELECT COUNT(*) FROM judge_submissions js2 WHERE js2.track = si.id) AS candidates
            FROM search_items si
            JOIN judge_submissions js ON js.track = si.id
            JOIN downloadable_files dlf ON dlf.id = js.query
            JOIN downloaded_file df ON df.filename = dlf.filename OR df.track = si.id
            WHERE si.track_id = ANY($1)
            ORDER BY si.track_id, (df.filename = dlf.filename) DESC, df.id DESC
            "#,
        )
        .bind::<Array<Text>, _>(&owned)
        .load::<ChosenRow>(&mut connection)?;
        for row in rows {
            if let Some(status) = statuses.get_mut(&row.track_id) {
                status.judging = Some(JudgingSummary {
                    outcome: "chosen",
                    uncertain: is_uncertain(row.label.as_deref(), row.score, threshold),
                    format: format_of(&row.filename),
                    label: row.label,
                    score: row.score,
                    filename: Some(row.filename),
                    username: Some(row.username),
                    size: Some(row.size),
                    candidates_checked: row.candidates,
                });
            }
        }
    }

    let failed: Vec<String> = statuses.iter().filter(|(_, s)| s.state == "failed").map(|(id, _)| id.clone()).collect();
    if !failed.is_empty() {
        let rows = diesel::sql_query(
            r#"
            SELECT si.track_id,
                (SELECT COUNT(*) FROM judge_submissions js WHERE js.track = si.id) AS candidates,
                (SELECT j.label FROM judgements j JOIN judge_submissions js ON j.submission = js.id
                 WHERE js.track = si.id AND j.label IS NOT NULL
                 GROUP BY j.label ORDER BY COUNT(*) DESC, j.label LIMIT 1) AS top_label,
                (SELECT MAX(js.score) FROM judge_submissions js WHERE js.track = si.id) AS best_score
            FROM search_items si
            WHERE si.track_id = ANY($1)
            "#,
        )
        .bind::<Array<Text>, _>(&failed)
        .load::<RejectedRow>(&mut connection)?;
        for row in rows {
            if let Some(status) = statuses.get_mut(&row.track_id) {
                status.judging = Some(JudgingSummary {
                    outcome: "rejected",
                    label: row.top_label,
                    score: row.best_score,
                    filename: None,
                    username: None,
                    size: None,
                    format: None,
                    uncertain: false,
                    candidates_checked: row.candidates,
                });
            }
        }
    }

    let request_ids: Vec<i32> = request_for.values().copied().collect::<HashSet<_>>().into_iter().collect();
    if !request_ids.is_empty() {
        let requesters: HashMap<i32, Requester> = diesel::sql_query(
            r#"
            SELECT dr.id AS request_id, u.id AS user_id, COALESCE(u.display_name, u.email) AS name, u.avatar_url
            FROM download_requests dr JOIN users u ON u.id = dr.user_id
            WHERE dr.id = ANY($1)
            "#,
        )
        .bind::<Array<Integer>, _>(&request_ids)
        .load::<RequesterRow>(&mut connection)?
        .into_iter()
        .map(|row| (row.request_id, Requester { id: row.user_id, name: row.name, avatar_url: row.avatar_url }))
        .collect();
        for (id, request) in request_for {
            if let (Some(status), Some(requester)) = (statuses.get_mut(&id), requesters.get(&request)) {
                status.requested_by = Some(requester.clone());
            }
        }
    }
    Ok(statuses)
}

#[derive(Serialize, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub total: usize,
    pub owned: usize,
    pub in_flight: usize,
    pub failed: usize,
    pub missing: usize,
    pub uncertain: usize,
}

pub fn count<'a>(statuses: impl IntoIterator<Item = &'a TrackStatus>) -> Counts {
    let mut counts = Counts::default();
    for status in statuses {
        counts.total += 1;
        match status.state {
            "owned" => {
                counts.owned += 1;
                if status.judging.as_ref().is_some_and(|summary| summary.uncertain) {
                    counts.uncertain += 1;
                }
            }
            "failed" => counts.failed += 1,
            "missing" => counts.missing += 1,
            _ => counts.in_flight += 1,
        }
    }
    counts
}

// ---------------------------------------------------------------------------------------------
// GET /collections/{kind}/{id}

#[derive(Deserialize)]
pub struct CollectionQuery {
    limit: Option<usize>,
    cursor: Option<usize>,
    since: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackEntry<'a> {
    #[serde(flatten)]
    track: &'a CollectionTrack,
    #[serde(flatten)]
    status: &'a TrackStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusChange<'a> {
    spotify_id: &'a str,
    #[serde(flatten)]
    status: &'a TrackStatus,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    user_id: i32,
    meta: CollectionMeta,
    ids: Vec<String>,
    signatures: Vec<String>,
}

fn signature(status: &TrackStatus) -> String {
    serde_json::to_string(status).unwrap_or_default()
}

fn save_snapshot(state: &AppState, snapshot: &Snapshot) -> String {
    let token = crate::auth::random_token(12);
    cache_set(state, &format!("collsnap:{token}"), snapshot, SNAPSHOT_TTL_SECS);
    token
}

pub fn owner_name(state: &AppState, user_id: i32) -> String {
    #[derive(QueryableByName)]
    struct Name {
        #[diesel(sql_type = Text)]
        name: String,
    }
    state
        .db_pool
        .get()
        .ok()
        .and_then(|mut con| {
            diesel::sql_query("SELECT COALESCE(display_name, email) AS name FROM users WHERE id = $1")
                .bind::<Integer, _>(user_id)
                .get_result::<Name>(&mut con)
                .ok()
        })
        .map(|row| row.name)
        .unwrap_or_default()
}

/// The folder a request for this collection downloads into (same rule as `POST /requests`).
pub fn download_folder(kind: CollectionKind, name: &str, owner: &str) -> Option<String> {
    let name = match kind {
        CollectionKind::Liked => format!("Liked Songs - {owner}"),
        _ => name.to_string(),
    };
    sanitize_folder_name(&name)
}

pub fn require_user(principal: &Principal) -> ApiResult<i32> {
    principal
        .user_id()
        .ok_or_else(|| ApiError::BadRequest("Service tokens have no Spotify account; log in to use this".into()))
}

#[get("/collections/{kind}/{id}")]
pub async fn collection(
    state: web::Data<AppState>,
    principal: Principal,
    path: web::Path<(String, String)>,
    query: web::Query<CollectionQuery>,
) -> ApiResult<HttpResponse> {
    let user_id = require_user(&principal)?;
    let (kind, id) = path.into_inner();
    let kind = CollectionKind::parse(&kind).ok_or(ApiError::NotFound("Unknown collection kind"))?;

    // Polling: only the tracks whose state changed since the last response.
    if let Some(token) = query.since.as_deref()
        && let Some(previous) = cache_get::<Snapshot>(&state, &format!("collsnap:{token}"))
        && previous.user_id == user_id
        && previous.meta.kind == kind
        && previous.meta.id == id
    {
        let statuses = track_statuses(&state, &previous.ids)?;
        let ordered: Vec<&TrackStatus> = previous.ids.iter().filter_map(|id| statuses.get(id)).collect();
        let signatures: Vec<String> = ordered.iter().map(|status| signature(status)).collect();
        let changes: Vec<StatusChange> = previous
            .ids
            .iter()
            .zip(&signatures)
            .zip(&previous.signatures)
            .filter(|((_, now), before)| now != before)
            .filter_map(|((id, _), _)| statuses.get(id).map(|status| StatusChange { spotify_id: id, status }))
            .collect();
        let counts = count(ordered.iter().copied());
        let body = serde_json::json!({
            "collection": previous.meta,
            "counts": counts,
            "changes": changes,
            "stateToken": "",
        });
        let token = save_snapshot(&state, &Snapshot { user_id, meta: previous.meta.clone(), ids: previous.ids.clone(), signatures });
        let mut body = body;
        body["stateToken"] = serde_json::Value::String(token);
        return Ok(HttpResponse::Ok().json(body));
    }

    let owner = owner_name(&state, user_id);
    let resolved = resolve(&state, user_id, &owner, kind, &id).await?;
    let ids: Vec<String> = resolved.tracks.iter().map(|track| track.spotify_id.clone()).collect();
    let statuses = track_statuses(&state, &ids)?;
    let missing = TrackStatus::missing();
    let ordered: Vec<&TrackStatus> = ids.iter().map(|id| statuses.get(id).unwrap_or(&missing)).collect();

    let limit = query.limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX);
    let start = query.cursor.unwrap_or(0).min(resolved.tracks.len());
    let end = (start + limit).min(resolved.tracks.len());
    let tracks: Vec<TrackEntry> = resolved.tracks[start..end]
        .iter()
        .zip(&ordered[start..end])
        .map(|(track, status)| TrackEntry { track, status })
        .collect();
    let token = save_snapshot(
        &state,
        &Snapshot {
            user_id,
            meta: resolved.meta.clone(),
            ids: ids.clone(),
            signatures: ordered.iter().map(|status| signature(status)).collect(),
        },
    );
    let folder = download_folder(kind, &resolved.meta.name, &owner);
    let folder_exists = folder
        .as_deref()
        .is_some_and(|folder| state.config.download_path.join(folder).is_dir());
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "collection": resolved.meta,
        "counts": count(ordered.iter().copied()),
        "folder": folder,
        "folderExists": folder_exists,
        "tracks": tracks,
        "nextCursor": (end < resolved.tracks.len()).then_some(end),
        "stateToken": token,
    })))
}

// ---------------------------------------------------------------------------------------------
// POST /collections/status

#[derive(Deserialize)]
pub struct StatusItem {
    kind: String,
    id: String,
}

#[derive(Deserialize)]
pub struct StatusBody {
    items: Vec<StatusItem>,
}

#[derive(Serialize, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BadgeCounts {
    pub total: usize,
    pub owned: usize,
    pub in_flight: usize,
    pub failed: usize,
}

pub fn badge_counts(ids: &[String], states: &HashMap<String, &'static str>) -> BadgeCounts {
    let mut counts = BadgeCounts { total: ids.len(), ..Default::default() };
    for id in ids {
        match states.get(id).copied().unwrap_or("missing") {
            "owned" => counts.owned += 1,
            "failed" => counts.failed += 1,
            "missing" => {}
            _ => counts.in_flight += 1,
        }
    }
    counts
}

/// Progress for the library rail: owned / in flight / failed per collection. Only collections
/// whose track list was read before (opened or requested) get counts; the rest are left out,
/// so this never calls Spotify.
#[post("/collections/status")]
pub async fn collections_status(
    state: web::Data<AppState>,
    principal: Principal,
    body: web::Json<StatusBody>,
) -> ApiResult<HttpResponse> {
    let user_id = require_user(&principal)?;
    if body.items.len() > STATUS_MAX_ITEMS {
        return Err(ApiError::BadRequest(format!("At most {STATUS_MAX_ITEMS} collections per request")));
    }
    let mut known: Vec<(String, Vec<String>)> = Vec::new();
    for item in &body.items {
        let Some(kind) = CollectionKind::parse(&item.kind) else { continue };
        if kind != CollectionKind::Liked && !valid_spotify_id(&item.id) {
            continue;
        }
        if let Some(ids) = cache_get::<Vec<String>>(&state, &ids_key(user_id, kind, &item.id)) {
            known.push((format!("{}:{}", kind.as_str(), item.id), ids));
        }
    }
    let mut all: Vec<String> = known.iter().flat_map(|(_, ids)| ids.iter().cloned()).collect();
    all.sort_unstable();
    all.dedup();
    let facts = track_facts(&state, &all)?;
    let live = live_requests(&state);
    let states: HashMap<String, &'static str> =
        facts.iter().map(|(id, fact)| (id.clone(), derive_state(fact, &live).0)).collect();
    let statuses: HashMap<String, BadgeCounts> =
        known.into_iter().map(|(key, ids)| (key, badge_counts(&ids, &states))).collect();
    Ok(HttpResponse::Ok().json(serde_json::json!({ "statuses": statuses })))
}

// ---------------------------------------------------------------------------------------------
// GET /search?q=

const SEARCH_LIMIT: u32 = 10;
const SEARCH_TTL_SECS: u64 = 600;

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct SearchAlbum {
    id: String,
    name: String,
    #[serde(default)]
    artists: Vec<NamedArtist>,
    #[serde(default)]
    images: Vec<Image>,
    #[serde(default)]
    total_tracks: u32,
}

#[derive(Deserialize)]
struct TrackTotal {
    total: u32,
}

#[derive(Deserialize)]
struct SearchPlaylist {
    id: String,
    name: String,
    owner: PlaylistOwner,
    #[serde(default)]
    images: Option<Vec<Image>>,
    tracks: Option<TrackTotal>,
}

#[derive(Deserialize)]
struct Items<T> {
    // Spotify returns `null` entries for playlists it can no longer show.
    items: Vec<Option<T>>,
}

#[derive(Deserialize)]
struct SearchPayload {
    tracks: Option<Items<FullTrack>>,
    albums: Option<Items<SearchAlbum>>,
    playlists: Option<Items<SearchPlaylist>>,
}

#[derive(Serialize, Deserialize)]
struct SearchResults {
    tracks: Vec<CollectionTrack>,
    albums: Vec<spotify_user::LibraryItem>,
    playlists: Vec<spotify_user::LibraryItem>,
}

fn present<T>(items: Option<Items<T>>) -> impl Iterator<Item = T> {
    items.map(|items| items.items).unwrap_or_default().into_iter().flatten()
}

fn search_results(payload: SearchPayload) -> SearchResults {
    SearchResults {
        tracks: present(payload.tracks).filter_map(|track| to_track(track, None)).collect(),
        albums: present(payload.albums)
            .map(|album: SearchAlbum| spotify_user::LibraryItem {
                kind: "album".into(),
                id: album.id,
                name: album.name,
                owner: join_artists(&album.artists),
                image_url: album.images.into_iter().next().map(|image| image.url),
                track_count: album.total_tracks,
            })
            .collect(),
        playlists: present(payload.playlists)
            .map(|playlist: SearchPlaylist| spotify_user::LibraryItem {
                kind: "playlist".into(),
                id: playlist.id,
                name: playlist.name,
                owner: playlist.owner.display_name.unwrap_or(playlist.owner.id),
                image_url: playlist.images.and_then(|images| images.into_iter().next()).map(|image| image.url),
                track_count: playlist.tracks.map(|tracks| tracks.total).unwrap_or(0),
            })
            .collect(),
    }
}

/// Spotify search (songs, albums, playlists) with the person's token. Songs carry their state, so
/// the results show what's already downloaded.
#[get("/search")]
pub async fn search(
    state: web::Data<AppState>,
    principal: Principal,
    query: web::Query<SearchQuery>,
) -> ApiResult<HttpResponse> {
    let user_id = require_user(&principal)?;
    let q = query.q.trim();
    if q.is_empty() || q.chars().count() > 200 {
        return Err(ApiError::BadRequest("q must be 1 to 200 characters".into()));
    }
    let cache_key = format!("search:{}", q.to_lowercase());
    let results = match cache_get::<SearchResults>(&state, &cache_key) {
        Some(results) => results,
        None => {
            let token = spotify_user::access_token_for(&state, user_id).await?;
            let mut url = reqwest::Url::parse(&format!("{API_BASE}/search")).expect("static URL");
            url.query_pairs_mut()
                .append_pair("q", q)
                .append_pair("type", "track,album,playlist")
                .append_pair("limit", &SEARCH_LIMIT.to_string());
            let payload: SearchPayload = spotify_user::spotify_get(&token, url.as_str()).await?;
            let results = search_results(payload);
            cache_set(&state, &cache_key, &results, SEARCH_TTL_SECS);
            results
        }
    };
    let ids: Vec<String> = results.tracks.iter().map(|track| track.spotify_id.clone()).collect();
    let statuses = track_statuses(&state, &ids)?;
    let missing = TrackStatus::missing();
    let tracks: Vec<TrackEntry> = results
        .tracks
        .iter()
        .map(|track| TrackEntry { track, status: statuses.get(&track.spotify_id).unwrap_or(&missing) })
        .collect();
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "tracks": tracks,
        "albums": results.albums,
        "playlists": results.playlists,
    })))
}

// ---------------------------------------------------------------------------------------------
// GET /tracks/by-spotify/{spotifyId}/judging

#[derive(QueryableByName)]
pub struct TrackRow {
    #[diesel(sql_type = Integer)]
    pub id: i32,
    #[diesel(sql_type = Text)]
    pub track_id: String,
    #[diesel(sql_type = Text)]
    pub title: String,
    #[diesel(sql_type = Text)]
    pub artist: String,
    #[diesel(sql_type = Text)]
    pub album: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub playlist_name: Option<String>,
}

pub fn track_by_spotify_id(connection: &mut PgConnection, spotify_id: &str) -> QueryResult<Option<TrackRow>> {
    diesel::sql_query(
        "SELECT id, track_id, track AS title, artist, album, playlist_name FROM search_items WHERE track_id = $1",
    )
    .bind::<Text, _>(spotify_id)
    .get_result::<TrackRow>(connection)
    .optional()
}

#[derive(QueryableByName)]
struct DetailCandidateRow {
    #[diesel(sql_type = Integer)]
    submission_id: i32,
    #[diesel(sql_type = Text)]
    username: String,
    #[diesel(sql_type = Text)]
    filename: String,
    #[diesel(sql_type = BigInt)]
    size: i64,
    #[diesel(sql_type = Nullable<diesel::sql_types::Float>)]
    score: Option<f32>,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    chosen: bool,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    rejected: bool,
}

#[derive(QueryableByName)]
struct RejectionRow {
    #[diesel(sql_type = Text)]
    reason: String,
    #[diesel(sql_type = Nullable<Text>)]
    value: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateDetail {
    id: i32,
    filename: String,
    username: String,
    size: i64,
    format: Option<String>,
    score: Option<f32>,
    label: Option<String>,
    chosen: bool,
    rejected: bool,
    judgements: Vec<JudgementResponse>,
}

#[get("/tracks/by-spotify/{spotify_id}/judging")]
pub async fn judging(
    state: web::Data<AppState>,
    _principal: Principal,
    path: web::Path<String>,
) -> ApiResult<HttpResponse> {
    let spotify_id = path.into_inner();
    if !valid_spotify_id(&spotify_id) {
        return Err(ApiError::BadRequest("Not a Spotify track id".into()));
    }
    let status = track_statuses(&state, std::slice::from_ref(&spotify_id))?
        .remove(&spotify_id)
        .unwrap_or_else(TrackStatus::missing);
    let mut connection = state.db_pool.get()?;
    let Some(track) = track_by_spotify_id(&mut connection, &spotify_id)? else {
        return Ok(HttpResponse::Ok().json(serde_json::json!({ "attempted": false, "status": status })));
    };

    let rows = diesel::sql_query(
        r#"
        SELECT js.id AS submission_id, dlf.username, dlf.filename, dlf.size, js.score,
            EXISTS (SELECT 1 FROM downloaded_file df WHERE df.filename = dlf.filename) AS chosen,
            EXISTS (SELECT 1 FROM rejected_track rt WHERE rt.track = js.id) AS rejected
        FROM judge_submissions js
        JOIN downloadable_files dlf ON dlf.id = js.query
        WHERE js.track = $1
        ORDER BY 6 DESC, js.score DESC NULLS LAST, js.id ASC
        "#,
    )
    .bind::<Integer, _>(track.id)
    .load::<DetailCandidateRow>(&mut connection)?;
    let verdicts = api::judgements_by_submission(&mut connection, track.id)?;
    let candidates: Vec<CandidateDetail> = rows
        .into_iter()
        .map(|row| {
            let judgements = verdicts.get(&row.submission_id).cloned().unwrap_or_default();
            CandidateDetail {
                id: row.submission_id,
                format: format_of(&row.filename),
                label: judgements.iter().find_map(|verdict| verdict.label.clone()),
                filename: row.filename,
                username: row.username,
                size: row.size,
                score: row.score,
                chosen: row.chosen,
                rejected: row.rejected,
                judgements,
            }
        })
        .collect();

    let rejections: Vec<String> = diesel::sql_query(
        r#"
        SELECT DISTINCT rt.reason::text AS reason, rt.value
        FROM rejected_track rt JOIN judge_submissions js ON rt.track = js.id
        WHERE js.track = $1
        "#,
    )
    .bind::<Integer, _>(track.id)
    .load::<RejectionRow>(&mut connection)?
    .into_iter()
    .filter_map(|row| api::format_reject_reason(Some(row.reason), row.value))
    .collect();

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "attempted": true,
        "status": status,
        "trackDbId": track.id,
        "title": track.title,
        "artist": track.artist,
        "album": track.album,
        // What the search manager sends to Soulseek; the relaxed second pass drops the artist.
        "query": format!("{} - {}", track.title, track.artist),
        "relaxedQuery": track.title,
        "candidates": candidates,
        "rejections": rejections,
        "uncertainBelow": uncertain_below(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_folder_matches_request_naming() {
        assert_eq!(download_folder(CollectionKind::Liked, "Liked Songs", "Lu").as_deref(), Some("Liked Songs - Lu"));
        assert_eq!(download_folder(CollectionKind::Playlist, "Rock/Metal: best", "x").as_deref(), Some("Rock Metal best"));
    }

    #[test]
    fn badge_counts_bucket_states() {
        let ids: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        let states: HashMap<String, &'static str> =
            [("a", "owned"), ("b", "queued"), ("c", "downloading"), ("d", "failed")].iter().map(|(k, v)| (k.to_string(), *v)).collect();
        assert_eq!(badge_counts(&ids, &states), BadgeCounts { total: 5, owned: 1, in_flight: 2, failed: 1 });
    }

    #[test]
    fn search_results_skip_null_playlists_and_local_tracks() {
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "tracks": { "items": [
                { "id": "t1", "name": "Nightcall", "duration_ms": 258000, "artists": [{ "name": "Kavinsky" }],
                  "album": { "name": "OutRun", "images": [{ "url": "http://img/1" }] }, "type": "track" },
                { "id": null, "name": "Local", "is_local": true }
            ]},
            "albums": { "items": [
                { "id": "a1", "name": "Kid A", "artists": [{ "name": "Radiohead" }], "images": [], "total_tracks": 10 }
            ]},
            "playlists": { "items": [
                null,
                { "id": "p1", "name": "Gym", "owner": { "id": "martin", "display_name": null }, "tracks": { "total": 31 } }
            ]}
        }))
        .unwrap();
        let results = search_results(payload);
        assert_eq!(results.tracks.len(), 1);
        assert_eq!(results.tracks[0].artist, "Kavinsky");
        assert_eq!(results.albums[0].owner, "Radiohead");
        assert_eq!(results.albums[0].track_count, 10);
        assert_eq!(results.playlists.len(), 1);
        assert_eq!(results.playlists[0].owner, "martin");
        assert_eq!(results.playlists[0].track_count, 31);
    }

    fn live(ids: &[i32]) -> HashSet<i32> {
        ids.iter().copied().collect()
    }

    #[test]
    fn owned_wins_over_everything() {
        let facts = TrackFacts { owned: true, rejections: 3, download_progress: Some(40), activity: Some("searching".into()), ..Default::default() };
        assert_eq!(derive_state(&facts, &live(&[])), ("owned", None));
    }

    #[test]
    fn retry_in_flight_is_not_failed() {
        // "Redbone" failed before and a retry is searching: searching, not failed.
        let facts = TrackFacts { rejections: 2, activity: Some("searching".into()), ..Default::default() };
        assert_eq!(derive_state(&facts, &live(&[])).0, "searching");
        let facts = TrackFacts { rejections: 2, activity: Some("judging".into()), ..Default::default() };
        assert_eq!(derive_state(&facts, &live(&[])).0, "comparing");
        let facts = TrackFacts { rejections: 2, download_progress: Some(63), ..Default::default() };
        assert_eq!(derive_state(&facts, &live(&[])), ("downloading", Some(63)));
    }

    #[test]
    fn pending_is_queued_only_while_the_request_is_live() {
        let facts = TrackFacts { rejections: 1, pending: Some((7, 1)), ..Default::default() };
        assert_eq!(derive_state(&facts, &live(&[7])).0, "queued");
        assert_eq!(derive_state(&facts, &live(&[8])).0, "failed");
        let never = TrackFacts { pending: Some((7, 0)), ..Default::default() };
        assert_eq!(derive_state(&never, &live(&[])).0, "missing");
    }

    #[test]
    fn new_rejection_after_queueing_means_failed_again() {
        let facts = TrackFacts { rejections: 2, pending: Some((7, 1)), ..Default::default() };
        assert_eq!(derive_state(&facts, &live(&[7])).0, "failed");
    }

    #[test]
    fn never_seen_is_missing() {
        assert_eq!(derive_state(&TrackFacts::default(), &live(&[])).0, "missing");
    }

    #[test]
    fn uncertain_by_label_or_score() {
        assert!(!is_uncertain(Some("exact"), Some(0.94), 0.75));
        assert!(is_uncertain(Some("wrong_version"), Some(0.94), 0.75));
        assert!(is_uncertain(None, Some(0.6), 0.75));
        assert!(!is_uncertain(None, None, 0.75));
    }

    #[test]
    fn counts_split_by_state() {
        let status = |state: &'static str, uncertain: bool| TrackStatus {
            state,
            progress: None,
            track_db_id: None,
            judging: (state == "owned").then_some(JudgingSummary {
                outcome: "chosen",
                label: None,
                score: None,
                filename: None,
                username: None,
                size: None,
                format: None,
                uncertain,
                candidates_checked: 1,
            }),
            requested_by: None,
        };
        let all = [status("owned", false), status("owned", true), status("queued", false), status("downloading", false), status("failed", false), status("missing", false)];
        assert_eq!(
            count(all.iter()),
            Counts { total: 6, owned: 2, in_flight: 2, failed: 1, missing: 1, uncertain: 1 }
        );
    }

    #[test]
    fn skips_tracks_that_cannot_be_searched() {
        let parse = |json: &str| to_track(serde_json::from_str::<FullTrack>(json).unwrap(), None);
        assert!(parse(r#"{"id":null,"name":"Local","is_local":true}"#).is_none());
        assert!(parse(r#"{"id":"ep1","name":"Episode","type":"episode"}"#).is_none());
        let track = parse(r#"{"id":"abc","name":"Redbone","duration_ms":327000,"type":"track","artists":[{"name":"Childish Gambino"}],"album":{"name":"Awaken","images":[{"url":"u"}]}}"#).unwrap();
        assert_eq!(track.artist, "Childish Gambino");
        assert_eq!(track.image_url.as_deref(), Some("u"));
    }

    #[test]
    fn formats_and_ids() {
        assert_eq!(format_of(r"Music\Album\06 Breathe Deeper.FLAC").as_deref(), Some("flac"));
        assert_eq!(format_of("no-extension"), None);
        assert!(valid_spotify_id("37i9dQZF1DX0SM0LYsmbMT"));
        assert!(!valid_spotify_id("abc/../x"));
        assert!(!valid_spotify_id(""));
    }
}

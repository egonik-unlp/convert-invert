//! "Descargadas": everything already on disk, for everyone, without needing access to the
//! Spotify playlist it came from (someone else's private playlist can't be opened with your
//! token). Songs come from the database and the files themselves; covers and durations from
//! whatever the server has already seen on Spotify, fetched per song only when never seen.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use actix_web::{HttpResponse, get, post, web};
use convert_invert::internals::worker::worker_manager::sanitize_folder_name;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer, Nullable, Text};
use futures_util::{StreamExt, stream};
use redis::Commands;
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::collections::{CachedCollection, CollectionMeta, CollectionTrack, Requester, format_of, valid_spotify_id};
use crate::errors::{ApiError, ApiResult};
use crate::mirror;
use crate::spotify_user::{self, API_BASE, Image};
use crate::state::AppState;

/// Tracks without a playlist tag (single songs, very old runs) group under this id.
pub const UNTAGGED: &str = "untagged";
const META_TTL_SECS: u64 = 180 * 24 * 3600;
/// A song Spotify couldn't describe is asked about again after this.
const MISS_TTL_SECS: u64 = 24 * 3600;
const SEEDED_KEY: &str = "trackmeta:seeded";
/// The full list is rebuilt at most this often (it stats every file).
const SNAPSHOT_TTL: Duration = Duration::from_secs(20);
const PAGE_DEFAULT: usize = 100;
const PAGE_MAX: usize = 300;
/// Spotify lookups for never-seen songs, per response.
const FETCH_PER_PAGE: usize = 60;

// ---------------------------------------------------------------------------------------------
// Song metadata (cover, duration), remembered from every collection the server resolves.

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct TrackMeta {
    pub image_url: Option<String>,
    pub duration_ms: Option<u64>,
}

fn meta_key(spotify_id: &str) -> String {
    format!("trackmeta:{spotify_id}")
}

/// Called whenever a collection is resolved: keep each song's cover and length for later.
pub fn remember(state: &AppState, _meta: &CollectionMeta, tracks: &[CollectionTrack]) {
    let Ok(mut con) = state.redis_pool.get() else { return };
    let mut pipe = redis::pipe();
    for track in tracks {
        let meta = TrackMeta { image_url: track.image_url.clone(), duration_ms: Some(track.duration_ms) };
        if let Ok(json) = serde_json::to_string(&meta) {
            pipe.set_ex(meta_key(&track.spotify_id), json, META_TTL_SECS).ignore();
        }
    }
    let _: redis::RedisResult<()> = pipe.query(&mut con);
}

/// Once per deployment: learn from the collections already sitting in the cache.
fn seed_from_cached_collections(state: &AppState) {
    let Ok(mut con) = state.redis_pool.get() else { return };
    if con.exists(SEEDED_KEY).unwrap_or(true) {
        return;
    }
    let keys: Vec<String> =
        con.scan_match("coll:*").map(|iter| iter.filter_map(Result::ok).collect()).unwrap_or_default();
    for key in keys {
        let cached: Option<String> = con.get(&key).ok();
        if let Some(collection) = cached.and_then(|json| serde_json::from_str::<CachedCollection>(&json).ok()) {
            remember(state, &collection.meta, &collection.tracks);
        }
    }
    let _: redis::RedisResult<()> = con.set_ex(SEEDED_KEY, "1", META_TTL_SECS);
}

fn known_meta(state: &AppState, ids: &[&str]) -> HashMap<String, TrackMeta> {
    let Ok(mut con) = state.redis_pool.get() else { return HashMap::new() };
    if ids.is_empty() {
        return HashMap::new();
    }
    let keys: Vec<String> = ids.iter().map(|id| meta_key(id)).collect();
    let values: Vec<Option<String>> = con.mget(&keys).unwrap_or_default();
    ids.iter()
        .zip(values)
        .filter_map(|(id, value)| Some((id.to_string(), serde_json::from_str(&value?).ok()?)))
        .collect()
}

#[derive(Deserialize)]
struct SpotifyTrack {
    duration_ms: u64,
    album: SpotifyAlbum,
}

#[derive(Deserialize)]
struct SpotifyAlbum {
    #[serde(default)]
    images: Vec<Image>,
}

/// Ask Spotify about songs the server has never resolved, a few at a time, and remember them
/// (misses too, for a day, so a song Spotify can't describe isn't asked about on every page).
async fn fetch_meta(state: &AppState, user_id: i32, ids: Vec<String>) -> HashMap<String, TrackMeta> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let Ok(token) = spotify_user::access_token_for(state, user_id).await else {
        return HashMap::new();
    };
    let found: Vec<(String, Option<TrackMeta>)> = stream::iter(ids)
        .map(|id| {
            let token = token.clone();
            async move {
                let track = spotify_user::spotify_get::<SpotifyTrack>(&token, &format!("{API_BASE}/tracks/{id}")).await;
                let meta = track.ok().map(|track| TrackMeta {
                    image_url: track.album.images.into_iter().next().map(|image| image.url),
                    duration_ms: Some(track.duration_ms),
                });
                (id, meta)
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;
    if let Ok(mut con) = state.redis_pool.get() {
        let mut pipe = redis::pipe();
        for (id, meta) in &found {
            let ttl = if meta.is_some() { META_TTL_SECS } else { MISS_TTL_SECS };
            let json = serde_json::to_string(&meta.clone().unwrap_or_default()).unwrap_or_default();
            pipe.set_ex(meta_key(id), json, ttl).ignore();
        }
        let _: redis::RedisResult<()> = pipe.query(&mut con);
    }
    found.into_iter().filter_map(|(id, meta)| Some((id, meta?))).collect()
}

// ---------------------------------------------------------------------------------------------
// The downloaded library, rebuilt from the database and the files at most every few seconds.

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedSong {
    pub spotify_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Seconds since the epoch: when the file arrived (or was requested, if the file is gone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// False when the database says downloaded but the file can't be found.
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<Requester>,
}

#[derive(Clone)]
struct Group {
    id: String,
    name: String,
    track_ids: Vec<String>,
}

#[derive(Clone)]
struct Snapshot {
    songs: Vec<DownloadedSong>,
    groups: Vec<Group>,
}

static SNAPSHOT: Mutex<Option<(Instant, Snapshot)>> = Mutex::new(None);

#[derive(QueryableByName)]
struct SongRow {
    #[diesel(sql_type = Text)]
    track_id: String,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Text)]
    artist: String,
    #[diesel(sql_type = Text)]
    album: String,
    #[diesel(sql_type = Integer)]
    df_id: i32,
    #[diesel(sql_type = Text)]
    filename: String,
    #[diesel(sql_type = Nullable<Text>)]
    local_path: Option<String>,
    #[diesel(sql_type = Nullable<Integer>)]
    user_id: Option<i32>,
    #[diesel(sql_type = Nullable<Text>)]
    user_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    avatar_url: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    requested_at: Option<i64>,
}

#[derive(QueryableByName)]
struct RequestRow {
    #[diesel(sql_type = Text)]
    source_kind: String,
    #[diesel(sql_type = Text)]
    source_id: String,
    #[diesel(sql_type = Text)]
    source_name: String,
}

/// The track ids the server last saw in each collection (`collids:*`, written by `resolve`).
fn cached_track_lists<'a>(state: &AppState, keys: impl Iterator<Item = &'a str>) -> Vec<Option<Vec<String>>> {
    let keys: Vec<&str> = keys.collect();
    if keys.is_empty() {
        return Vec::new();
    }
    let Ok(mut con) = state.redis_pool.get() else { return vec![None; keys.len()] };
    let values: Vec<Option<String>> = con.mget(&keys).unwrap_or_else(|_| vec![None; keys.len()]);
    values.into_iter().map(|value| serde_json::from_str(&value?).ok()).collect()
}

#[derive(QueryableByName)]
struct MembershipRow {
    #[diesel(sql_type = Text)]
    track_id: String,
    #[diesel(sql_type = Text)]
    playlist_id: String,
    #[diesel(sql_type = Nullable<Text>)]
    playlist_name: Option<String>,
}

fn build_snapshot(state: &AppState) -> ApiResult<Snapshot> {
    let root = state.config.download_path.clone();
    let mut connection = state.db_pool.get()?;
    let rows = diesel::sql_query(
        r#"
        SELECT DISTINCT ON (si.track_id)
            si.track_id, si.track AS title, si.artist, si.album,
            df.id AS df_id, df.filename, df.local_path,
            u.id AS user_id, COALESCE(u.display_name, u.email) AS user_name, u.avatar_url,
            EXTRACT(EPOCH FROM dr.created_at)::BIGINT AS requested_at
        FROM downloaded_file df
        JOIN search_items si ON si.id = df.track
        LEFT JOIN download_requests dr ON dr.id = si.request_id
        LEFT JOIN users u ON u.id = dr.user_id
        ORDER BY si.track_id, (df.local_path IS NULL), df.id DESC
        "#,
    )
    .load::<SongRow>(&mut connection)?;

    let mut songs = Vec::with_capacity(rows.len());
    for row in rows {
        if !valid_spotify_id(&row.track_id) {
            continue;
        }
        let file = mirror::locate(&root, row.df_id, &row.filename, row.local_path.as_deref(), &mut connection);
        let metadata = file.as_deref().and_then(|path| std::fs::metadata(path).ok());
        let modified = metadata
            .as_ref()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|age| age.as_secs() as i64);
        songs.push(DownloadedSong {
            format: format_of(&file.as_deref().map(|path| path.to_string_lossy().into_owned()).unwrap_or(row.filename)),
            spotify_id: row.track_id,
            title: row.title,
            artist: row.artist,
            album: row.album,
            image_url: None,
            duration_ms: None,
            size: metadata.as_ref().map(std::fs::Metadata::len),
            added_at: modified.or(row.requested_at),
            available: metadata.is_some(),
            requested_by: match (row.user_id, row.user_name) {
                (Some(id), Some(name)) => Some(Requester { id, name, avatar_url: row.avatar_url }),
                _ => None,
            },
        });
    }

    let owned: HashSet<&str> = songs.iter().map(|song| song.spotify_id.as_str()).collect();
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut add = |id: &str, name: Option<String>, track_id: &str| {
        let group = groups
            .entry(id.to_string())
            .or_insert_with(|| Group { id: id.to_string(), name: String::new(), track_ids: Vec::new() });
        if group.name.is_empty()
            && let Some(name) = name.filter(|name| !name.is_empty())
        {
            group.name = name;
        }
        if owned.contains(track_id) && !group.track_ids.iter().any(|known| known == track_id) {
            group.track_ids.push(track_id.to_string());
        }
    };

    // Where each song was first downloaded (a song is one row, so this is a single collection).
    let memberships = diesel::sql_query(
        "SELECT track_id, COALESCE(playlist_id, '') AS playlist_id, playlist_name FROM search_items",
    )
    .load::<MembershipRow>(&mut connection)?;
    for row in memberships.iter().filter(|row| owned.contains(row.track_id.as_str())) {
        let id = if row.playlist_id.is_empty() { UNTAGGED } else { row.playlist_id.as_str() };
        add(id, row.playlist_name.clone(), &row.track_id);
    }

    // Every collection someone requested, with its full track list when the server has it
    // cached, so a song shows under each collection that contains it (and a collection whose
    // songs were all downloaded before it was requested still gets a card).
    let requests = diesel::sql_query(
        r#"
        SELECT DISTINCT ON (source_kind, source_id) source_kind, source_id, source_name
        FROM download_requests
        WHERE source_kind IN ('playlist', 'album', 'liked')
        ORDER BY source_kind, source_id, created_at DESC
        "#,
    )
    .load::<RequestRow>(&mut connection)?;
    let mut candidates: Vec<(String, Option<String>, String)> = requests
        .into_iter()
        .map(|row| {
            let key = match row.source_kind.as_str() {
                "liked" => format!("collids:liked:{}", row.source_id.trim_start_matches("liked")),
                kind => format!("collids:{kind}:{}", row.source_id),
            };
            (row.source_id, Some(row.source_name), key)
        })
        .collect();
    let tagged: Vec<String> = memberships
        .iter()
        .filter(|row| !row.playlist_id.is_empty())
        .map(|row| row.playlist_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for id in tagged {
        candidates.push((id.clone(), None, format!("collids:playlist:{id}")));
        candidates.push((id.clone(), None, format!("collids:album:{id}")));
    }
    let lists = cached_track_lists(state, candidates.iter().map(|(_, _, key)| key.as_str()));
    for ((id, name, _), ids) in candidates.into_iter().zip(lists) {
        if name.is_some() {
            add(&id, name.clone(), "");
        }
        for track_id in ids.unwrap_or_default() {
            add(&id, None, &track_id);
        }
    }

    groups.retain(|_, group| !group.track_ids.is_empty());
    Ok(Snapshot { songs, groups: groups.into_values().collect() })
}

fn snapshot(state: &AppState) -> ApiResult<Snapshot> {
    if let Ok(guard) = SNAPSHOT.lock()
        && let Some((built, snapshot)) = guard.as_ref()
        && built.elapsed() < SNAPSHOT_TTL
    {
        return Ok(snapshot.clone());
    }
    let fresh = build_snapshot(state)?;
    if let Ok(mut guard) = SNAPSHOT.lock() {
        *guard = Some((Instant::now(), fresh.clone()));
    }
    Ok(fresh)
}

async fn snapshot_async(state: &web::Data<AppState>) -> ApiResult<Snapshot> {
    let state = state.clone();
    web::block(move || {
        seed_from_cached_collections(&state);
        snapshot(&state)
    })
    .await
    .map_err(|err| ApiError::Internal(err.to_string()))?
}

/// Folder a group's songs were filed into (same rule as the workers: the sanitized name).
fn group_folder(group: &Group) -> Option<String> {
    if group.id == UNTAGGED {
        return None;
    }
    sanitize_folder_name(&group.name).or_else(|| sanitize_folder_name(&group.id))
}

fn display_name(group: &Group) -> String {
    if group.name.is_empty() { group.id.clone() } else { group.name.clone() }
}

// ---------------------------------------------------------------------------------------------
// GET /downloaded/songs

#[derive(Deserialize)]
pub struct SongsQuery {
    q: Option<String>,
    sort: Option<String>,
    collection: Option<String>,
    cursor: Option<usize>,
    limit: Option<usize>,
}

/// Every word of the query must appear in the title, artist or album.
pub fn matches(song: &DownloadedSong, query: &str) -> bool {
    let haystack = format!("{} {} {}", song.title, song.artist, song.album).to_lowercase();
    query.to_lowercase().split_whitespace().all(|word| haystack.contains(word))
}

pub fn sort_songs(songs: &mut [DownloadedSong], sort: &str) {
    let key = |value: &str| value.to_lowercase();
    match sort {
        "artist" => songs.sort_by_key(|s| (key(&s.artist), key(&s.album), key(&s.title))),
        "album" => songs.sort_by_key(|s| (key(&s.album), key(&s.artist), key(&s.title))),
        "title" => songs.sort_by_key(|s| (key(&s.title), key(&s.artist))),
        _ => songs.sort_by_key(|s| std::cmp::Reverse(s.added_at.unwrap_or(0))),
    }
}

#[get("/downloaded/songs")]
pub async fn downloaded_songs(
    state: web::Data<AppState>,
    principal: Principal,
    query: web::Query<SongsQuery>,
) -> ApiResult<HttpResponse> {
    let snapshot = snapshot_async(&state).await?;
    let total_all = snapshot.songs.len();
    let total_bytes: u64 = snapshot.songs.iter().filter_map(|song| song.size).sum();

    let mut songs = snapshot.songs;
    let mut collection = None;
    if let Some(id) = query.collection.as_deref() {
        let group = snapshot
            .groups
            .iter()
            .find(|group| group.id == id)
            .ok_or(ApiError::NotFound("No downloaded collection with that id"))?;
        let wanted: HashSet<&str> = group.track_ids.iter().map(String::as_str).collect();
        songs.retain(|song| wanted.contains(song.spotify_id.as_str()));
        let folder = group_folder(group);
        collection = Some(serde_json::json!({
            "id": group.id,
            "name": display_name(group),
            "folder": folder,
            "folderExists": folder.as_deref().is_some_and(|folder| state.config.download_path.join(folder).is_dir()),
        }));
    }
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        songs.retain(|song| matches(song, q));
    }
    sort_songs(&mut songs, query.sort.as_deref().unwrap_or("recent"));

    let total = songs.len();
    let limit = query.limit.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX);
    let start = query.cursor.unwrap_or(0).min(total);
    let end = (start + limit).min(total);
    let mut page: Vec<DownloadedSong> = songs[start..end].to_vec();
    attach_meta(&state, principal.user_id(), &mut page).await;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "songs": page,
        "total": total,
        "totalAll": total_all,
        "totalBytes": total_bytes,
        "nextCursor": (end < total).then_some(end),
        "collection": collection,
    })))
}

async fn attach_meta(state: &AppState, user_id: Option<i32>, songs: &mut [DownloadedSong]) {
    let ids: Vec<&str> = songs.iter().map(|song| song.spotify_id.as_str()).collect();
    let mut meta = known_meta(state, &ids);
    if let Some(user_id) = user_id {
        let unknown: Vec<String> = ids
            .iter()
            .filter(|id| !meta.contains_key(**id))
            .take(FETCH_PER_PAGE)
            .map(|id| id.to_string())
            .collect();
        meta.extend(fetch_meta(state, user_id, unknown).await);
    }
    for song in songs {
        if let Some(found) = meta.get(&song.spotify_id) {
            song.image_url = found.image_url.clone();
            song.duration_ms = found.duration_ms.filter(|ms| *ms > 0);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// GET /downloaded/collections

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadedCollection {
    id: String,
    name: String,
    /// How many of its songs are downloaded.
    songs: usize,
    /// Up to four covers for the mosaic.
    covers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
}

#[get("/downloaded/collections")]
pub async fn downloaded_collections(state: web::Data<AppState>, principal: Principal) -> ApiResult<HttpResponse> {
    let snapshot = snapshot_async(&state).await?;
    let by_id: HashMap<&str, &DownloadedSong> =
        snapshot.songs.iter().map(|song| (song.spotify_id.as_str(), song)).collect();

    let mut groups = snapshot.groups.clone();
    groups.sort_by_key(|group| {
        std::cmp::Reverse(group.track_ids.iter().filter_map(|id| by_id.get(id.as_str())?.added_at).max().unwrap_or(0))
    });

    // Covers: the first songs of each group with a known cover, fetching a few unknown ones.
    let first_ids: Vec<&str> = groups
        .iter()
        .flat_map(|group| group.track_ids.iter().take(8).map(String::as_str))
        .collect();
    let mut meta = known_meta(&state, &first_ids);
    if let Some(user_id) = principal.user_id() {
        let unknown: Vec<String> = groups
            .iter()
            .flat_map(|group| group.track_ids.iter().take(4))
            .filter(|id| !meta.contains_key(id.as_str()))
            .take(FETCH_PER_PAGE)
            .cloned()
            .collect();
        meta.extend(fetch_meta(&state, user_id, unknown).await);
    }

    let list: Vec<DownloadedCollection> = groups
        .iter()
        .map(|group| {
            let mut covers: Vec<String> = Vec::new();
            for id in &group.track_ids {
                if let Some(url) = meta.get(id).and_then(|meta| meta.image_url.clone())
                    && !covers.contains(&url)
                {
                    covers.push(url);
                }
                if covers.len() == 4 {
                    break;
                }
            }
            DownloadedCollection {
                id: group.id.clone(),
                name: display_name(group),
                songs: group.track_ids.len(),
                covers,
                updated_at: group.track_ids.iter().filter_map(|id| by_id.get(id.as_str())?.added_at).max(),
                folder: group_folder(group),
            }
        })
        .collect();
    Ok(HttpResponse::Ok().json(serde_json::json!({ "collections": list })))
}

// ---------------------------------------------------------------------------------------------
// POST /downloaded/collections/{id}/folder

/// Before a downloaded collection's .zip: link its songs that were filed elsewhere into its
/// folder (a song in two playlists lives in the first one's folder).
#[post("/downloaded/collections/{id}/folder")]
pub async fn fill_downloaded_folder(
    state: web::Data<AppState>,
    _principal: Principal,
    path: web::Path<String>,
) -> ApiResult<HttpResponse> {
    let id = path.into_inner();
    let snapshot = snapshot_async(&state).await?;
    let group = snapshot
        .groups
        .iter()
        .find(|group| group.id == id)
        .ok_or(ApiError::NotFound("No downloaded collection with that id"))?
        .clone();
    let folder = group_folder(&group).ok_or_else(|| ApiError::BadRequest("These songs have no folder".into()))?;
    let state_for_mirror = state.clone();
    let folder_for_mirror = folder.clone();
    let linked = web::block(move || mirror::mirror_into(&state_for_mirror, &folder_for_mirror, &group.track_ids))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))??;
    let exists = state.config.download_path.join(&folder).is_dir();
    Ok(HttpResponse::Ok().json(serde_json::json!({ "folder": folder, "folderExists": exists, "linked": linked })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(title: &str, artist: &str, album: &str, added_at: i64) -> DownloadedSong {
        DownloadedSong {
            spotify_id: title.into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            image_url: None,
            duration_ms: None,
            size: None,
            added_at: Some(added_at),
            format: None,
            available: true,
            requested_by: None,
        }
    }

    #[test]
    fn every_word_must_match_somewhere() {
        let s = song("Everything in Its Right Place", "Radiohead", "Kid A", 0);
        assert!(matches(&s, "radiohead kid"));
        assert!(matches(&s, "RIGHT place"));
        assert!(!matches(&s, "radiohead amnesiac"));
    }

    #[test]
    fn sorts_recent_first_or_by_artist() {
        let mut songs = vec![song("b", "Zed", "x", 1), song("a", "Abba", "y", 3), song("c", "Moby", "z", 2)];
        sort_songs(&mut songs, "recent");
        assert_eq!(songs.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["a", "c", "b"]);
        sort_songs(&mut songs, "artist");
        assert_eq!(songs.iter().map(|s| s.artist.as_str()).collect::<Vec<_>>(), ["Abba", "Moby", "Zed"]);
    }

    #[test]
    fn untagged_songs_have_no_folder() {
        let group = Group { id: UNTAGGED.into(), name: String::new(), track_ids: vec![] };
        assert_eq!(group_folder(&group), None);
        let group = Group { id: "abc".into(), name: "de fondo".into(), track_ids: vec![] };
        assert_eq!(group_folder(&group).as_deref(), Some("de fondo"));
    }
}

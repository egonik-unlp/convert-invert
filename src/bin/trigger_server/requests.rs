//! "Download what's missing": requests for a collection (or a selection of it), the shared queue
//! with who asked for what, and picking a rejected candidate by hand.

use std::collections::{HashMap, HashSet};

use actix_web::{HttpResponse, get, post, web};
use convert_invert::internals::nexttrack::{self, SeedTrack};
use convert_invert::internals::query::query_manager::{ResourceKind, fill_albums};
use convert_invert::internals::search::search_manager::{DownloadableFile, JudgeSubmission, SearchItem};
use convert_invert::internals::utils::config::config_manager::Config;
use convert_invert::internals::worker::worker_manager::{PreResolved, WorkerStartOptions, request_run_id_prefix};
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer, Nullable, Text};
use redis::Commands;
use serde::{Deserialize, Serialize};

use crate::api;
use crate::auth::Principal;
use crate::collections::{
    self, CollectionKind, CollectionTrack, PENDING_TTL_SECS, TrackStatus, pending_key, require_user, track_facts,
    track_statuses, valid_spotify_id,
};
use crate::errors::{ApiError, ApiResult};
use crate::state::AppState;

const DEFAULT_CHUNK_SIZE: usize = 15;
const MAX_WORKERS: usize = 32;

#[derive(Deserialize)]
pub struct Source {
    kind: String,
    id: String,
}

#[derive(Deserialize, Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RequestOptions {
    #[serde(default)]
    enhanced_retries: bool,
    extend_count: Option<usize>,
    /// Admin only; members always get the server default.
    worker_count: Option<usize>,
    /// Admin only.
    chunk_size: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestBody {
    source: Source,
    track_ids: Option<Vec<String>>,
    #[serde(default)]
    options: RequestOptions,
}

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = Integer)]
    id: i32,
}

/// Pick what a request should queue: tracks that are missing or failed, optionally limited to a
/// selection, in collection order and without duplicates.
pub fn tracks_to_queue<'a>(
    tracks: &'a [CollectionTrack],
    statuses: &HashMap<String, TrackStatus>,
    selection: Option<&HashSet<String>>,
) -> Vec<&'a CollectionTrack> {
    let mut seen = HashSet::new();
    tracks
        .iter()
        .filter(|track| selection.is_none_or(|ids| ids.contains(&track.spotify_id)))
        .filter(|track| statuses.get(&track.spotify_id).is_none_or(TrackStatus::needs_download))
        .filter(|track| seen.insert(track.spotify_id.as_str()))
        .collect()
}

/// Clamp what a caller may override: members get the defaults, admins get bounded values.
pub fn effective_run_shape(options: &RequestOptions, is_admin: bool, default_workers: usize) -> (usize, usize) {
    if !is_admin {
        return (default_workers.max(1), DEFAULT_CHUNK_SIZE);
    }
    (
        options.worker_count.unwrap_or(default_workers).clamp(1, MAX_WORKERS),
        options.chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE).clamp(1, 1000),
    )
}

fn search_item(track: &CollectionTrack) -> SearchItem {
    SearchItem::new(track.spotify_id.clone(), track.title.clone(), track.album.clone(), track.artist.clone())
}

fn mark_pending(state: &AppState, request_id: i32, items: &[SearchItem]) -> ApiResult<()> {
    let ids: Vec<String> = items.iter().map(|item| item.track_id.clone()).collect();
    let facts = track_facts(state, &ids)?;
    let mut con = state.redis_pool.get()?;
    let mut pipe = redis::pipe();
    for (position, item) in items.iter().enumerate() {
        let key = pending_key(&item.track_id);
        let rejections = facts.get(&item.track_id).map(|fact| fact.rejections).unwrap_or(0);
        pipe.del(&key)
            .hset_multiple(
                &key,
                &[
                    ("request_id", request_id.to_string()),
                    ("rejections", rejections.to_string()),
                    ("position", position.to_string()),
                ],
            )
            .expire(&key, PENDING_TTL_SECS as i64)
            .ignore();
    }
    let _: () = pipe.query(&mut con)?;
    Ok(())
}

fn insert_request(
    state: &AppState,
    user_id: Option<i32>,
    kind: &str,
    source_id: &str,
    name: &str,
    options: &RequestOptions,
    track_count: usize,
) -> ApiResult<i32> {
    let options = serde_json::to_string(options).map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok(diesel::sql_query(
        r#"
        INSERT INTO download_requests (user_id, source_kind, source_id, source_name, options, track_count)
        VALUES ($1, $2, $3, $4, $5::jsonb, $6) RETURNING id
        "#,
    )
    .bind::<Nullable<Integer>, _>(user_id)
    .bind::<Text, _>(kind)
    .bind::<Text, _>(source_id)
    .bind::<Text, _>(name)
    .bind::<Text, _>(options)
    .bind::<Integer, _>(track_count as i32)
    .get_result::<IdRow>(&mut state.db_pool.get()?)?
    .id)
}

/// Continue the collection with `count` tracks from the next-track model, using every track of
/// the collection (not just the missing ones) as seeds.
async fn extension(state: &AppState, seeds: &[CollectionTrack], count: usize) -> ApiResult<Vec<SearchItem>> {
    let seed_tracks: Vec<SeedTrack> = seeds
        .iter()
        .map(|track| SeedTrack { track_id: track.spotify_id.clone(), name: track.title.clone(), artist: track.artist.clone() })
        .collect();
    let count = count.min(nexttrack::MAX_EXTEND_COUNT);
    let extended = tokio::task::spawn_blocking(move || {
        nexttrack::shared_model()?.extend(&seed_tracks, count, nexttrack::DEFAULT_ARTIST_PENALTY)
    })
    .await
    .map_err(|err| ApiError::Internal(format!("Next-track task: {err}")))?
    .map_err(|err| ApiError::BadRequest(format!("Could not extend this collection: {err}")))?;
    let mut items: Vec<SearchItem> = extended
        .tracks
        .iter()
        .map(|entry| SearchItem::new(entry.track_id().to_string(), entry.name.clone(), String::new(), entry.artist.clone()))
        .collect();
    if let (Some(id), Some(secret)) = (state.config.spotify_client_id.clone(), state.config.spotify_client_secret.clone()) {
        fill_albums(&mut items, id, secret).await;
    }
    Ok(items)
}

fn base_config() -> ApiResult<Config> {
    Config::try_from_env().map_err(|err| ApiError::Internal(format!("Failed to load worker config: {err}")))
}

#[post("/requests")]
pub async fn create_request(
    state: web::Data<AppState>,
    principal: Principal,
    body: web::Json<RequestBody>,
) -> ApiResult<HttpResponse> {
    let user_id = require_user(&principal)?;
    let body = body.into_inner();
    let kind = CollectionKind::parse(&body.source.kind).ok_or_else(|| ApiError::BadRequest("Unknown collection kind".into()))?;
    if let Some(ids) = &body.track_ids
        && ids.iter().any(|id| !valid_spotify_id(id))
    {
        return Err(ApiError::BadRequest("trackIds must be Spotify track ids".into()));
    }

    let owner = collections::owner_name(&state, user_id);
    let resolved = collections::resolve(&state, user_id, &owner, kind, &body.source.id).await?;
    let ids: Vec<String> = resolved.tracks.iter().map(|track| track.spotify_id.clone()).collect();
    let statuses = track_statuses(&state, &ids)?;
    let selection = body.track_ids.as_ref().map(|ids| ids.iter().cloned().collect::<HashSet<_>>());
    let mut items: Vec<SearchItem> = tracks_to_queue(&resolved.tracks, &statuses, selection.as_ref())
        .into_iter()
        .map(search_item)
        .collect();

    let mut name = match kind {
        CollectionKind::Liked => format!("Liked Songs - {owner}"),
        _ => resolved.meta.name.clone(),
    };
    if let Some(count) = body.options.extend_count.filter(|count| *count > 0) {
        let generated = extension(&state, &resolved.tracks, count).await?;
        let generated_ids: Vec<String> = generated.iter().map(|item| item.track_id.clone()).collect();
        let generated_statuses = track_statuses(&state, &generated_ids)?;
        let known: HashSet<String> = items.iter().map(|item| item.track_id.clone()).collect();
        items.extend(generated.into_iter().filter(|item| {
            !known.contains(&item.track_id)
                && generated_statuses.get(&item.track_id).is_none_or(TrackStatus::needs_download)
        }));
        name = format!("{name} (extended)");
    }

    if items.is_empty() {
        return Ok(HttpResponse::Ok().json(serde_json::json!({ "requestId": null, "queued": 0 })));
    }

    // The library tag must stay base62 so the per-playlist views can address it.
    let library_tag = match kind {
        CollectionKind::Liked => format!("liked{user_id}"),
        _ => resolved.meta.id.clone(),
    };
    let request_id = insert_request(&state, Some(user_id), kind.as_str(), &library_tag, &name, &body.options, items.len())?;
    mark_pending(&state, request_id, &items)?;

    let (worker_count, chunk_size) = effective_run_shape(&body.options, principal.is_admin(), state.config.worker_count);
    let queued = items.len();
    let spawned = state
        .worker_supervisor
        .start(
            WorkerStartOptions {
                worker_count,
                username_prefix: state.config.username_prefix.clone(),
                port_base: state.config.port_base,
                run_id_prefix: request_run_id_prefix(request_id),
                account_mode: state.config.worker_account_mode.clone(),
                playlist_id: library_tag,
                resource_kind: match kind {
                    CollectionKind::Album => ResourceKind::Album,
                    CollectionKind::Track => ResourceKind::Track,
                    CollectionKind::Playlist | CollectionKind::Liked => ResourceKind::Playlist,
                },
                chunk_size,
                playlist_range: None,
                random_order: false,
                enhanced_retries: body.options.enhanced_retries,
                extend_count: None,
                pre_resolved: Some(PreResolved { name, items }),
                request_id: Some(request_id),
            },
            base_config()?,
            state.config.worker_password.clone(),
        )
        .await
        .map_err(|err| ApiError::Internal(format!("Failed to start workers: {err}")))?;
    tracing::info!(request_id, user_id, queued, workers = spawned.len(), kind = kind.as_str(), "Download request started");
    Ok(HttpResponse::Ok().json(serde_json::json!({ "requestId": request_id, "queued": queued })))
}

// ---------------------------------------------------------------------------------------------
// GET /queue

#[derive(QueryableByName)]
struct QueueTrackRow {
    #[diesel(sql_type = Integer)]
    id: i32,
    #[diesel(sql_type = Text)]
    track_id: String,
    #[diesel(sql_type = Text)]
    title: String,
    #[diesel(sql_type = Text)]
    artist: String,
    #[diesel(sql_type = Text)]
    album: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QueueItem {
    spotify_id: String,
    title: String,
    artist: String,
    album: String,
    #[serde(flatten)]
    status: TrackStatus,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct QueueResponse {
    downloading: Vec<QueueItem>,
    comparing: Vec<QueueItem>,
    searching: Vec<QueueItem>,
    queued: Vec<QueueItem>,
}

/// Everything in flight across all requests, grouped by stage, with who asked for each track.
#[get("/queue")]
pub async fn queue(state: web::Data<AppState>, _principal: Principal) -> ApiResult<HttpResponse> {
    let mut connection = state.db_pool.get()?;

    // Candidates: active downloads (by db id), activity markers and pending markers (by Spotify id).
    let active_db_ids: Vec<i32> = api::collect_active_downloads(&state.redis_pool)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|download| download.track_db_id)
        .collect();
    let mut spotify_ids: Vec<String> = api::activity_markers(&state.redis_pool)
        .unwrap_or_default()
        .into_iter()
        .map(|marker| marker.track_id)
        .collect();
    let mut positions: HashMap<String, (i32, i64)> = HashMap::new();
    {
        let mut con = state.redis_pool.get()?;
        let keys: Vec<String> = con.scan_match::<_, String>("req:pending:*")?.collect::<Result<Vec<String>, _>>()?;
        for key in keys {
            let marker: HashMap<String, String> = con.hgetall(&key).unwrap_or_default();
            let spotify_id = key.trim_start_matches("req:pending:").to_string();
            let request = marker.get("request_id").and_then(|v| v.parse().ok()).unwrap_or(i32::MAX);
            let position = marker.get("position").and_then(|v| v.parse().ok()).unwrap_or(0);
            positions.insert(spotify_id.clone(), (request, position));
            spotify_ids.push(spotify_id);
        }
    }
    let rows = diesel::sql_query(
        "SELECT id, track_id, track AS title, artist, album FROM search_items WHERE id = ANY($1) OR track_id = ANY($2)",
    )
    .bind::<diesel::sql_types::Array<Integer>, _>(&active_db_ids)
    .bind::<diesel::sql_types::Array<Text>, _>(&spotify_ids)
    .load::<QueueTrackRow>(&mut connection)?;
    drop(connection);

    let ids: Vec<String> = rows.iter().map(|row| row.track_id.clone()).collect::<HashSet<_>>().into_iter().collect();
    let mut statuses = track_statuses(&state, &ids)?;
    let mut response = QueueResponse::default();
    for row in rows {
        let Some(status) = statuses.remove(&row.track_id) else { continue };
        let bucket = match status.state {
            "downloading" => &mut response.downloading,
            "comparing" => &mut response.comparing,
            "searching" => &mut response.searching,
            "queued" => &mut response.queued,
            _ => continue,
        };
        let _ = row.id;
        bucket.push(QueueItem { spotify_id: row.track_id, title: row.title, artist: row.artist, album: row.album, status });
    }
    response.downloading.sort_by_key(|item| std::cmp::Reverse(item.status.progress.unwrap_or(0)));
    response
        .queued
        .sort_by_key(|item| positions.get(&item.spotify_id).copied().unwrap_or((i32::MAX, 0)));
    Ok(HttpResponse::Ok().json(response))
}

// ---------------------------------------------------------------------------------------------
// POST /tracks/by-spotify/{spotifyId}/accept

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptBody {
    candidate_id: i32,
}

#[derive(QueryableByName)]
struct CandidateRow {
    #[diesel(sql_type = Text)]
    filename: String,
    #[diesel(sql_type = Text)]
    username: String,
    #[diesel(sql_type = BigInt)]
    size: i64,
    #[diesel(sql_type = Integer)]
    file_id: i32,
    #[diesel(sql_type = Nullable<diesel::sql_types::Float>)]
    score: Option<f32>,
}

/// Download a candidate the judge rejected, because a person looked at it and said it's fine.
/// Recorded as a `manual` verdict so the judging history shows who overrode the judge.
#[post("/tracks/by-spotify/{spotify_id}/accept")]
pub async fn accept_candidate(
    state: web::Data<AppState>,
    principal: Principal,
    path: web::Path<String>,
    body: web::Json<AcceptBody>,
) -> ApiResult<HttpResponse> {
    let user_id = require_user(&principal)?;
    let spotify_id = path.into_inner();
    if !valid_spotify_id(&spotify_id) {
        return Err(ApiError::BadRequest("Not a Spotify track id".into()));
    }
    let status = track_statuses(&state, std::slice::from_ref(&spotify_id))?
        .remove(&spotify_id)
        .unwrap_or_else(unreachable_missing);
    if status.state == "owned" {
        return Err(ApiError::BadRequest("This song is already downloaded".into()));
    }
    if status.in_flight() {
        return Err(ApiError::BadRequest("This song is already on its way".into()));
    }

    let mut connection = state.db_pool.get()?;
    let track = collections::track_by_spotify_id(&mut connection, &spotify_id)?.ok_or(ApiError::NotFound("Track not found"))?;
    let candidate = diesel::sql_query(
        r#"
        SELECT dlf.filename, dlf.username, dlf.size, dlf.id AS file_id, js.score
        FROM judge_submissions js JOIN downloadable_files dlf ON dlf.id = js.query
        WHERE js.id = $1 AND js.track = $2
        "#,
    )
    .bind::<Integer, _>(body.candidate_id)
    .bind::<Integer, _>(track.id)
    .get_result::<CandidateRow>(&mut connection)
    .optional()?
    .ok_or(ApiError::NotFound("That file isn't a candidate for this song"))?;

    // `by` is shown in the judging panel ("Elegido a mano por …").
    let raw = serde_json::json!({ "acceptedBy": user_id, "by": collections::owner_name(&state, user_id) }).to_string();
    diesel::sql_query(
        r#"
        INSERT INTO judgements (submission, judge, label, score, accepted, raw)
        VALUES ($1, 'manual', 'exact', 1.0, true, $2)
        ON CONFLICT (submission, judge) DO UPDATE SET accepted = true, label = 'exact', raw = EXCLUDED.raw, created_at = now()
        "#,
    )
    .bind::<Integer, _>(body.candidate_id)
    .bind::<Text, _>(&raw)
    .execute(&mut connection)?;
    diesel::sql_query("INSERT INTO manual_accepts (track, downloadable_file, user_id) VALUES ($1, $2, $3)")
        .bind::<Integer, _>(track.id)
        .bind::<Integer, _>(candidate.file_id)
        .bind::<Integer, _>(user_id)
        .execute(&mut connection)?;
    drop(connection);

    let folder = track.playlist_name.clone().unwrap_or_default();
    let request_id = insert_request(&state, Some(user_id), "track", &spotify_id, &track.title, &RequestOptions::default(), 1)?;
    let item = SearchItem::new(track.track_id.clone(), track.title.clone(), track.album.clone(), track.artist.clone());
    mark_pending(&state, request_id, std::slice::from_ref(&item))?;
    diesel::sql_query("UPDATE search_items SET request_id = $1 WHERE id = $2")
        .bind::<Integer, _>(request_id)
        .bind::<Integer, _>(track.id)
        .execute(&mut state.db_pool.get()?)?;

    let mut submission = JudgeSubmission::new(
        item,
        DownloadableFile { filename: candidate.filename, username: candidate.username, size: candidate.size },
    );
    submission.score = candidate.score;
    state
        .worker_supervisor
        .start_forced_download(
            submission,
            folder,
            base_config()?,
            state.config.worker_password.clone(),
            &state.config.username_prefix,
            &state.config.worker_account_mode,
            state.config.port_base,
            &request_run_id_prefix(request_id),
        )
        .map_err(|err| ApiError::Internal(format!("Failed to start the download: {err}")))?;
    tracing::info!(request_id, user_id, track = %spotify_id, candidate = body.candidate_id, "Manually accepted candidate");
    Ok(HttpResponse::Ok().json(serde_json::json!({ "requestId": request_id })))
}

fn unreachable_missing() -> TrackStatus {
    TrackStatus { state: "missing", progress: None, track_db_id: None, judging: None, requested_by: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> CollectionTrack {
        CollectionTrack { spotify_id: id.into(), title: id.into(), artist: "a".into(), album: "b".into(), duration_ms: 1, image_url: None }
    }

    fn status(state: &'static str) -> TrackStatus {
        TrackStatus { state, progress: None, track_db_id: None, judging: None, requested_by: None }
    }

    #[test]
    fn queues_missing_and_failed_only() {
        let tracks = vec![track("a"), track("b"), track("c"), track("d"), track("e"), track("a")];
        let statuses: HashMap<String, TrackStatus> = [
            ("a".to_string(), status("missing")),
            ("b".to_string(), status("owned")),
            ("c".to_string(), status("failed")),
            ("d".to_string(), status("searching")),
        ]
        .into_iter()
        .collect();
        let picked: Vec<&str> = tracks_to_queue(&tracks, &statuses, None).iter().map(|t| t.spotify_id.as_str()).collect();
        // "e" has no status yet (never seen): missing. Duplicate "a" is queued once.
        assert_eq!(picked, vec!["a", "c", "e"]);
    }

    #[test]
    fn selection_limits_the_request() {
        let tracks = vec![track("a"), track("b"), track("c")];
        let statuses = HashMap::new();
        let selection: HashSet<String> = ["c".to_string(), "a".to_string()].into_iter().collect();
        let picked: Vec<&str> = tracks_to_queue(&tracks, &statuses, Some(&selection)).iter().map(|t| t.spotify_id.as_str()).collect();
        assert_eq!(picked, vec!["a", "c"]);
    }

    #[test]
    fn nothing_to_queue_when_everything_is_owned() {
        let tracks = vec![track("a")];
        let statuses: HashMap<String, TrackStatus> = [("a".to_string(), status("owned"))].into_iter().collect();
        assert!(tracks_to_queue(&tracks, &statuses, None).is_empty());
    }

    #[test]
    fn members_cannot_override_run_shape() {
        let options = RequestOptions { worker_count: Some(8), chunk_size: Some(3), ..Default::default() };
        assert_eq!(effective_run_shape(&options, false, 2), (2, DEFAULT_CHUNK_SIZE));
        assert_eq!(effective_run_shape(&options, true, 2), (8, 3));
        let wild = RequestOptions { worker_count: Some(500), chunk_size: Some(0), ..Default::default() };
        assert_eq!(effective_run_shape(&wild, true, 2), (MAX_WORKERS, 1));
    }
}

//! Spotify on behalf of a logged-in person: the OAuth token exchange, per-user tokens (encrypted at
//! rest, refreshed on demand) and the person's library. The workers keep using client credentials;
//! only this module ever holds a user's token.

use std::sync::OnceLock;
use std::time::Duration;

use actix_web::{HttpResponse, get, web};
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use diesel::prelude::*;
use diesel::sql_types::{Bool, Bytea, Double, Integer, Nullable, Text};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::config::AppConfig;
use crate::errors::{ApiError, ApiResult};
use crate::state::AppState;

pub const SCOPES: &str = "user-read-email user-read-private playlist-read-private playlist-read-collaborative user-library-read";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
pub const API_BASE: &str = "https://api.spotify.com/v1";
const LIBRARY_TTL_SECS: u64 = 300;
const NONCE_LEN: usize = 12;

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build Spotify HTTP client")
    })
}

#[derive(Debug)]
pub enum SpotifyError {
    /// The person revoked the app or the refresh token expired; they have to log in again.
    Revoked,
    /// No tokens stored for this user (never logged in, or access was removed).
    NotLinked,
    /// Spotify failed, rate-limited us, or answered something we couldn't parse.
    Upstream(String),
}

impl From<SpotifyError> for ApiError {
    fn from(error: SpotifyError) -> Self {
        match error {
            SpotifyError::Revoked | SpotifyError::NotLinked => ApiError::Unauthorized,
            SpotifyError::Upstream(message) => ApiError::BadGateway(message),
        }
    }
}

impl From<reqwest::Error> for SpotifyError {
    fn from(error: reqwest::Error) -> Self {
        SpotifyError::Upstream(format!("Spotify request failed: {error}"))
    }
}

// ---------------------------------------------------------------------------------------------
// Token encryption

pub struct TokenCipher(Aes256Gcm);

impl TokenCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)))
    }

    /// `nonce || ciphertext`, with a fresh random nonce per call.
    pub fn encrypt(&self, plaintext: &str) -> Vec<u8> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .0
            .encrypt(&nonce, plaintext.as_bytes())
            .expect("AES-GCM encryption cannot fail for in-memory buffers");
        let mut out = nonce.to_vec();
        out.extend(ciphertext);
        out
    }

    pub fn decrypt(&self, data: &[u8]) -> anyhow::Result<String> {
        if data.len() <= NONCE_LEN {
            anyhow::bail!("encrypted token is too short");
        }
        let (nonce, ciphertext) = data.split_at(NONCE_LEN);
        let plaintext = self
            .0
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow::anyhow!("could not decrypt token (wrong TOKEN_ENC_KEY?)"))?;
        Ok(String::from_utf8(plaintext)?)
    }
}

// ---------------------------------------------------------------------------------------------
// OAuth

pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str, code_challenge: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("static URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", state)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", code_challenge);
    url.to_string()
}

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: i64,
    /// Always present on the code exchange; Spotify may omit it on refresh (keep the old one).
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: String,
}

fn form_body(params: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://localhost/").expect("static URL");
    url.query_pairs_mut().extend_pairs(params);
    url.query().unwrap_or_default().to_string()
}

async fn token_request(config: &AppConfig, params: &[(&str, &str)]) -> Result<TokenResponse, SpotifyError> {
    let (client_id, client_secret, _) = config.spotify_oauth().map_err(SpotifyError::Upstream)?;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{client_secret}"));
    let response = http()
        .post(TOKEN_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form_body(params))
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if status.is_success() {
        return serde_json::from_str(&text)
            .map_err(|err| SpotifyError::Upstream(format!("Unexpected token response: {err}")));
    }
    if text.contains("invalid_grant") {
        return Err(SpotifyError::Revoked);
    }
    Err(SpotifyError::Upstream(format!("Spotify token endpoint answered {status}")))
}

pub async fn exchange_code(
    config: &AppConfig,
    code: &str,
    code_verifier: &str,
) -> Result<TokenResponse, SpotifyError> {
    let (_, _, redirect_uri) = config.spotify_oauth().map_err(SpotifyError::Upstream)?;
    token_request(
        config,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
        ],
    )
    .await
}

pub async fn spotify_get<T: DeserializeOwned>(access_token: &str, url: &str) -> Result<T, SpotifyError> {
    let response = http().get(url).bearer_auth(access_token).send().await?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(SpotifyError::Upstream("Spotify is rate limiting requests".into()));
    }
    let text = response.text().await?;
    if !status.is_success() {
        return Err(SpotifyError::Upstream(format!("Spotify answered {status} for {url}")));
    }
    serde_json::from_str(&text).map_err(|err| SpotifyError::Upstream(format!("Unexpected Spotify response: {err}")))
}

#[derive(Deserialize)]
pub struct Image {
    pub url: String,
}

#[derive(Deserialize)]
pub struct SpotifyMe {
    pub id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

pub async fn fetch_me(access_token: &str) -> Result<SpotifyMe, SpotifyError> {
    spotify_get(access_token, &format!("{API_BASE}/me")).await
}

// ---------------------------------------------------------------------------------------------
// Stored tokens

pub fn store_tokens(
    connection: &mut PgConnection,
    cipher: &TokenCipher,
    user_id: i32,
    tokens: &TokenResponse,
) -> QueryResult<usize> {
    let refresh = tokens.refresh_token.as_deref().map(|token| cipher.encrypt(token));
    let access = cipher.encrypt(&tokens.access_token);
    diesel::sql_query(
        r#"
        INSERT INTO spotify_tokens (user_id, refresh_token_enc, access_token_enc, access_expires_at, scopes)
        VALUES ($1, COALESCE($2, ''::bytea), $3, now() + make_interval(secs => $4), $5)
        ON CONFLICT (user_id) DO UPDATE SET
            refresh_token_enc = COALESCE($2, spotify_tokens.refresh_token_enc),
            access_token_enc = EXCLUDED.access_token_enc,
            access_expires_at = EXCLUDED.access_expires_at,
            scopes = CASE WHEN EXCLUDED.scopes = '' THEN spotify_tokens.scopes ELSE EXCLUDED.scopes END
        "#,
    )
    .bind::<Integer, _>(user_id)
    .bind::<Nullable<Bytea>, _>(refresh)
    .bind::<Bytea, _>(access)
    .bind::<Double, _>(tokens.expires_in as f64)
    .bind::<Text, _>(&tokens.scope)
    .execute(connection)
}

/// Drop a user's sessions and tokens, so the next request asks them to log in again.
pub fn revoke_user_access(connection: &mut PgConnection, user_id: i32) -> QueryResult<()> {
    diesel::sql_query("DELETE FROM sessions WHERE user_id = $1")
        .bind::<Integer, _>(user_id)
        .execute(connection)?;
    diesel::sql_query("DELETE FROM spotify_tokens WHERE user_id = $1")
        .bind::<Integer, _>(user_id)
        .execute(connection)?;
    Ok(())
}

#[derive(QueryableByName)]
struct TokenRow {
    #[diesel(sql_type = Bytea)]
    refresh_token_enc: Vec<u8>,
    #[diesel(sql_type = Nullable<Bytea>)]
    access_token_enc: Option<Vec<u8>>,
    #[diesel(sql_type = Bool)]
    fresh: bool,
}

fn load_tokens(connection: &mut PgConnection, user_id: i32) -> Result<TokenRow, SpotifyError> {
    diesel::sql_query(
        r#"
        SELECT refresh_token_enc, access_token_enc,
               COALESCE(access_expires_at > now() + interval '60 seconds', false) AS fresh
        FROM spotify_tokens WHERE user_id = $1
        "#,
    )
    .bind::<Integer, _>(user_id)
    .get_result::<TokenRow>(connection)
    .optional()
    .map_err(|err| SpotifyError::Upstream(err.to_string()))?
    .ok_or(SpotifyError::NotLinked)
}

fn fresh_access(row: &TokenRow, cipher: &TokenCipher) -> Option<String> {
    row.fresh
        .then_some(row.access_token_enc.as_deref())
        .flatten()
        .and_then(|data| cipher.decrypt(data).ok())
}

/// A valid access token for `user_id`, refreshing it when it expires within a minute. A Redis lock
/// keeps two concurrent requests from both spending the refresh token.
pub async fn access_token_for(state: &AppState, user_id: i32) -> Result<String, SpotifyError> {
    let cipher = TokenCipher::new(&state.config.token_key);
    let db_error = |err: diesel::r2d2::PoolError| SpotifyError::Upstream(err.to_string());
    let row = load_tokens(&mut *state.db_pool.get().map_err(db_error)?, user_id)?;
    if let Some(token) = fresh_access(&row, &cipher) {
        return Ok(token);
    }

    let lock_key = format!("spotify:refresh_lock:{user_id}");
    let mut locked = false;
    for _ in 0..20 {
        let acquired: Option<String> = state.redis_pool.get().ok().and_then(|mut con| {
            redis::cmd("SET")
                .arg(&lock_key)
                .arg("1")
                .arg("NX")
                .arg("EX")
                .arg(15)
                .query(&mut con)
                .ok()
        });
        if acquired.is_some() {
            locked = true;
            break;
        }
        // Someone else is refreshing; use their result once it lands.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let row = load_tokens(&mut *state.db_pool.get().map_err(db_error)?, user_id)?;
        if let Some(token) = fresh_access(&row, &cipher) {
            return Ok(token);
        }
    }

    let result = refresh(state, &cipher, user_id, &row).await;
    if locked && let Ok(mut con) = state.redis_pool.get() {
        let _: Result<i64, _> = redis::cmd("DEL").arg(&lock_key).query(&mut con);
    }
    result
}

async fn refresh(
    state: &AppState,
    cipher: &TokenCipher,
    user_id: i32,
    row: &TokenRow,
) -> Result<String, SpotifyError> {
    let refresh_token = cipher
        .decrypt(&row.refresh_token_enc)
        .map_err(|_| SpotifyError::Revoked)?;
    let mut connection = state
        .db_pool
        .get()
        .map_err(|err| SpotifyError::Upstream(err.to_string()))?;
    match token_request(&state.config, &[("grant_type", "refresh_token"), ("refresh_token", &refresh_token)]).await {
        Ok(tokens) => {
            store_tokens(&mut connection, cipher, user_id, &tokens)
                .map_err(|err| SpotifyError::Upstream(err.to_string()))?;
            Ok(tokens.access_token)
        }
        Err(SpotifyError::Revoked) => {
            tracing::warn!(user_id, "Spotify refresh token was revoked; closing the user's sessions");
            revoke_user_access(&mut connection, user_id)
                .map_err(|err| SpotifyError::Upstream(err.to_string()))?;
            Err(SpotifyError::Revoked)
        }
        Err(other) => Err(other),
    }
}

// ---------------------------------------------------------------------------------------------
// Library

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItem {
    /// "liked" | "playlist" | "album"
    pub kind: String,
    pub id: String,
    pub name: String,
    pub owner: String,
    pub image_url: Option<String>,
    pub track_count: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryResponse {
    pub items: Vec<LibraryItem>,
    /// True when Spotify could not be reached and this is the last copy we had.
    pub stale: bool,
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    next: Option<String>,
    #[serde(default)]
    total: u32,
}

#[derive(Deserialize)]
struct Owner {
    display_name: Option<String>,
    id: String,
}

#[derive(Deserialize)]
struct TrackTotal {
    total: u32,
}

#[derive(Deserialize)]
struct SimplePlaylist {
    id: String,
    name: String,
    owner: Owner,
    #[serde(default)]
    images: Option<Vec<Image>>,
    tracks: Option<TrackTotal>,
}

#[derive(Deserialize)]
struct Artist {
    name: String,
}

#[derive(Deserialize)]
struct SimpleAlbum {
    id: String,
    name: String,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    images: Vec<Image>,
    #[serde(default)]
    total_tracks: u32,
}

#[derive(Deserialize)]
struct SavedAlbum {
    album: SimpleAlbum,
}

async fn all_pages<T: DeserializeOwned>(access_token: &str, first_url: String) -> Result<Vec<T>, SpotifyError> {
    let mut items = Vec::new();
    let mut next = Some(first_url);
    // Hard stop at 40 pages (2,000 items) so a huge library can't stall the request.
    for _ in 0..40 {
        let Some(url) = next.take() else { break };
        let page: Page<T> = spotify_get(access_token, &url).await?;
        items.extend(page.items);
        next = page.next;
    }
    Ok(items)
}

pub async fn fetch_library(access_token: &str, owner_name: &str) -> Result<Vec<LibraryItem>, SpotifyError> {
    let liked: Page<serde_json::Value> = spotify_get(access_token, &format!("{API_BASE}/me/tracks?limit=1")).await?;
    let playlists: Vec<SimplePlaylist> = all_pages(access_token, format!("{API_BASE}/me/playlists?limit=50")).await?;
    let albums: Vec<SavedAlbum> = all_pages(access_token, format!("{API_BASE}/me/albums?limit=50")).await?;

    let mut items = vec![LibraryItem {
        kind: "liked".into(),
        id: "liked".into(),
        name: "Liked Songs".into(),
        owner: owner_name.to_string(),
        image_url: None,
        track_count: liked.total,
    }];
    items.extend(playlists.into_iter().map(|playlist| LibraryItem {
        kind: "playlist".into(),
        id: playlist.id,
        name: playlist.name,
        owner: playlist.owner.display_name.unwrap_or(playlist.owner.id),
        image_url: playlist.images.and_then(|images| images.into_iter().next()).map(|image| image.url),
        track_count: playlist.tracks.map(|tracks| tracks.total).unwrap_or(0),
    }));
    items.extend(albums.into_iter().map(|saved| LibraryItem {
        kind: "album".into(),
        id: saved.album.id,
        name: saved.album.name,
        owner: saved
            .album
            .artists
            .into_iter()
            .map(|artist| artist.name)
            .collect::<Vec<_>>()
            .join(", "),
        image_url: saved.album.images.into_iter().next().map(|image| image.url),
        track_count: saved.album.total_tracks,
    }));
    Ok(items)
}

#[derive(Deserialize)]
pub struct LibraryQuery {
    refresh: Option<String>,
}

#[derive(QueryableByName)]
struct NameRow {
    #[diesel(sql_type = Text)]
    name: String,
}

fn redis_get(state: &AppState, key: &str) -> Option<String> {
    let mut con = state.redis_pool.get().ok()?;
    redis::cmd("GET").arg(key).query::<Option<String>>(&mut con).ok().flatten()
}

fn redis_set(state: &AppState, key: &str, value: &str, ttl_secs: Option<u64>) {
    let Ok(mut con) = state.redis_pool.get() else { return };
    let mut cmd = redis::cmd("SET");
    cmd.arg(key).arg(value);
    if let Some(ttl) = ttl_secs {
        cmd.arg("EX").arg(ttl);
    }
    let _: Result<String, _> = cmd.query(&mut con);
}

/// The logged-in person's Spotify library: Liked Songs first, then playlists, then saved albums.
#[get("/me/library")]
pub async fn library(
    state: web::Data<AppState>,
    principal: Principal,
    query: web::Query<LibraryQuery>,
) -> ApiResult<HttpResponse> {
    let Some(user_id) = principal.user_id() else {
        return Err(ApiError::BadRequest("Service tokens have no Spotify library".into()));
    };
    let cache_key = format!("library:{user_id}");
    let last_key = format!("library:{user_id}:last");
    let force = matches!(query.refresh.as_deref(), Some("1" | "true"));

    if !force && let Some(cached) = redis_get(&state, &cache_key) {
        return Ok(HttpResponse::Ok().content_type("application/json").body(cached));
    }

    let owner = diesel::sql_query("SELECT COALESCE(display_name, email) AS name FROM users WHERE id = $1")
        .bind::<Integer, _>(user_id)
        .get_result::<NameRow>(&mut state.db_pool.get()?)
        .map(|row| row.name)
        .unwrap_or_default();

    let fetched = match access_token_for(&state, user_id).await {
        Ok(token) => fetch_library(&token, &owner).await,
        Err(err) => Err(err),
    };
    match fetched {
        Ok(items) => {
            let body = serde_json::to_string(&LibraryResponse { items, stale: false })
                .map_err(|err| ApiError::Internal(err.to_string()))?;
            redis_set(&state, &cache_key, &body, Some(LIBRARY_TTL_SECS));
            redis_set(&state, &last_key, &body, None);
            Ok(HttpResponse::Ok().content_type("application/json").body(body))
        }
        Err(SpotifyError::Upstream(message)) => {
            let Some(last) = redis_get(&state, &last_key)
                .and_then(|body| serde_json::from_str::<LibraryResponse>(&body).ok())
            else {
                return Err(ApiError::BadGateway(format!("Could not read your Spotify library: {message}")));
            };
            tracing::warn!(user_id, %message, "Serving a stale Spotify library");
            Ok(HttpResponse::Ok().json(LibraryResponse { items: last.items, stale: true }))
        }
        Err(other) => Err(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_cipher_round_trips_and_detects_tampering() {
        let cipher = TokenCipher::new(&[3; 32]);
        let sealed = cipher.encrypt("refresh-token");
        assert_eq!(cipher.decrypt(&sealed).unwrap(), "refresh-token");
        assert_ne!(cipher.encrypt("refresh-token"), sealed, "nonce must differ per call");

        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(cipher.decrypt(&tampered).is_err());
        assert!(TokenCipher::new(&[4; 32]).decrypt(&sealed).is_err());
        assert!(cipher.decrypt(&[0; 5]).is_err());
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let url = authorize_url("client", "http://localhost:5173/api/auth/callback", "st", "challenge");
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["state"], "st");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["code_challenge"], "challenge");
        assert_eq!(pairs["redirect_uri"], "http://localhost:5173/api/auth/callback");
        assert!(pairs["scope"].contains("user-library-read"));
    }

    #[test]
    fn form_body_encodes_reserved_characters() {
        assert_eq!(form_body(&[("a", "x y"), ("b", "1&2=3")]), "a=x+y&b=1%262%3D3");
    }

    #[test]
    fn library_parses_playlist_without_tracks_field() {
        let playlist: SimplePlaylist =
            serde_json::from_str(r#"{"id":"p","name":"Gym","owner":{"id":"martin","display_name":null},"images":null}"#).unwrap();
        assert!(playlist.tracks.is_none());
        assert!(playlist.images.is_none());
    }
}

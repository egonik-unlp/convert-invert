//! Login with Spotify, browser sessions, roles and the invite list.
//!
//! People log in through Spotify (Authorization Code + PKCE, exchanged server-side). Only invited
//! emails — or the ones in `ADMIN_EMAILS` — get a session. The session is an opaque random token in
//! an HttpOnly cookie; the database only keeps its sha256.

use std::future::{Ready, ready};

use actix_web::cookie::time::Duration as CookieDuration;
use actix_web::cookie::{Cookie, SameSite};
use actix_web::dev::Payload;
use actix_web::http::header::LOCATION;
use actix_web::{FromRequest, HttpMessage, HttpRequest, HttpResponse, get, patch, post, web};
use base64::Engine;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::errors::{ApiError, ApiResult};
use crate::spotify_user::{self, TokenCipher};
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "ci_session";
const OAUTH_COOKIE: &str = "ci_oauth";
const SESSION_DAYS: i64 = 30;
const OAUTH_TTL_SECS: u64 = 600;

// ---------------------------------------------------------------------------------------------
// Principal

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }
}

/// Who is making the request. Put in the request extensions by `middleware::require_auth`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    User { id: i32, role: Role },
    /// A script holding `API_KEY`. Has admin rights and no Spotify account.
    Service,
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        matches!(self, Principal::Service | Principal::User { role: Role::Admin, .. })
    }

    pub fn user_id(&self) -> Option<i32> {
        match self {
            Principal::User { id, .. } => Some(*id),
            Principal::Service => None,
        }
    }
}

impl FromRequest for Principal {
    type Error = ApiError;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(req.extensions().get::<Principal>().cloned().ok_or(ApiError::Unauthorized))
    }
}

/// Extractor for operational endpoints: 403 unless the caller is an admin (or the service token).
pub struct RequireAdmin(pub Principal);

impl FromRequest for RequireAdmin {
    type Error = ApiError;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(match req.extensions().get::<Principal>().cloned() {
            Some(principal) if principal.is_admin() => Ok(RequireAdmin(principal)),
            Some(_) => Err(ApiError::Forbidden("Only admins can do this")),
            None => Err(ApiError::Unauthorized),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Tokens and cookies

pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// RFC 7636 S256 code challenge.
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Only in-app hash routes survive the round trip through Spotify; anything else is dropped so the
/// login can't be turned into an open redirect.
pub fn sanitize_return_to(value: Option<&str>) -> String {
    match value {
        Some(hash)
            if hash.starts_with('#')
                && hash.len() <= 200
                && hash.chars().all(|c| c.is_ascii_graphic()) =>
        {
            hash.to_string()
        }
        _ => String::new(),
    }
}

fn cookie<'a>(name: &'a str, value: String, path: &'a str, max_age: CookieDuration, secure: bool) -> Cookie<'a> {
    Cookie::build(name, value)
        .path(path)
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(max_age)
        .finish()
}

fn redirect(location: &str) -> actix_web::HttpResponseBuilder {
    let mut builder = HttpResponse::Found();
    builder.insert_header((LOCATION, location.to_string()));
    builder
}

// ---------------------------------------------------------------------------------------------
// Sessions

#[derive(QueryableByName)]
struct SessionRow {
    #[diesel(sql_type = Integer)]
    user_id: i32,
    #[diesel(sql_type = Text)]
    role: String,
    #[diesel(sql_type = Bool)]
    stale: bool,
}

/// Resolve a session cookie to a principal, sliding its expiry at most once an hour.
pub fn session_principal(connection: &mut PgConnection, token: &str) -> QueryResult<Option<Principal>> {
    let token_hash = hash_token(token);
    let Some(row) = diesel::sql_query(
        r#"
        SELECT s.user_id, u.role, (s.last_used_at < now() - interval '1 hour') AS stale
        FROM sessions s JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = $1 AND s.expires_at > now() AND u.status = 'active'
        "#,
    )
    .bind::<Text, _>(&token_hash)
    .get_result::<SessionRow>(connection)
    .optional()?
    else {
        return Ok(None);
    };

    if row.stale {
        diesel::sql_query(format!(
            "UPDATE sessions SET last_used_at = now(), expires_at = now() + interval '{SESSION_DAYS} days' WHERE token_hash = $1"
        ))
        .bind::<Text, _>(&token_hash)
        .execute(connection)?;
        diesel::sql_query("UPDATE users SET last_seen_at = now() WHERE id = $1")
            .bind::<Integer, _>(row.user_id)
            .execute(connection)?;
    }
    Ok(Role::parse(&row.role).map(|role| Principal::User { id: row.user_id, role }))
}

fn create_session(connection: &mut PgConnection, user_id: i32, user_agent: Option<&str>) -> QueryResult<String> {
    let token = random_token(32);
    diesel::sql_query(format!(
        "INSERT INTO sessions (user_id, token_hash, user_agent, expires_at) VALUES ($1, $2, $3, now() + interval '{SESSION_DAYS} days')"
    ))
    .bind::<Integer, _>(user_id)
    .bind::<Text, _>(hash_token(&token))
    .bind::<Nullable<Text>, _>(user_agent.map(|agent| agent.chars().take(300).collect::<String>()))
    .execute(connection)?;
    Ok(token)
}

// ---------------------------------------------------------------------------------------------
// Login

#[derive(Serialize, Deserialize)]
struct PendingLogin {
    verifier: String,
    return_to: String,
}

#[derive(Deserialize)]
pub struct LoginQuery {
    return_to: Option<String>,
}

#[get("/auth/login")]
pub async fn login(state: web::Data<AppState>, query: web::Query<LoginQuery>) -> ApiResult<HttpResponse> {
    let (client_id, _, redirect_uri) = state.config.spotify_oauth().map_err(ApiError::Internal)?;
    let oauth_state = random_token(24);
    let verifier = random_token(48);
    let pending = PendingLogin {
        verifier: verifier.clone(),
        return_to: sanitize_return_to(query.return_to.as_deref()),
    };
    let mut con = state.redis_pool.get()?;
    let _: String = redis::cmd("SET")
        .arg(format!("oauth:{oauth_state}"))
        .arg(serde_json::to_string(&pending).map_err(|err| ApiError::Internal(err.to_string()))?)
        .arg("EX")
        .arg(OAUTH_TTL_SECS)
        .query(&mut con)?;

    let url = spotify_user::authorize_url(client_id, redirect_uri, &oauth_state, &pkce_challenge(&verifier));
    Ok(redirect(&url)
        .cookie(cookie(
            OAUTH_COOKIE,
            oauth_state,
            "/api/auth",
            CookieDuration::seconds(OAUTH_TTL_SECS as i64),
            state.config.cookie_secure,
        ))
        .finish())
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum LoginError {
    /// Missing or mismatched `state`: the login wasn't started from this browser.
    BadState,
    NotInvited,
    Spotify(String),
}

/// What an existing `users` row looks like to the login decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExistingUser {
    pub id: i32,
    pub status: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginDecision {
    /// Create a new admin from `ADMIN_EMAILS`.
    CreateAdmin,
    /// Activate (or refresh) this user, promoting to admin when `promote` is set.
    Activate { id: i32, promote: bool },
    Reject,
}

/// Pure login policy: invited or already active users get in, admin emails always get in (and are
/// promoted), disabled users never do.
pub fn decide_login(existing: Option<&ExistingUser>, is_admin_email: bool) -> LoginDecision {
    match existing {
        Some(user) if user.status == "disabled" => LoginDecision::Reject,
        Some(user) => LoginDecision::Activate { id: user.id, promote: is_admin_email },
        None if is_admin_email => LoginDecision::CreateAdmin,
        None => LoginDecision::Reject,
    }
}

#[derive(QueryableByName)]
struct UserLookupRow {
    #[diesel(sql_type = Integer)]
    id: i32,
    #[diesel(sql_type = Text)]
    status: String,
}

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = Integer)]
    id: i32,
}

async fn complete_login(
    state: &AppState,
    request: &HttpRequest,
    query: &CallbackQuery,
) -> Result<(String, String), LoginError> {
    if let Some(error) = &query.error {
        return Err(LoginError::Spotify(format!("Spotify returned {error}")));
    }
    let (Some(code), Some(oauth_state)) = (&query.code, &query.state) else {
        return Err(LoginError::BadState);
    };
    let cookie_state = request.cookie(OAUTH_COOKIE).map(|cookie| cookie.value().to_string());
    if cookie_state.as_deref() != Some(oauth_state.as_str()) {
        return Err(LoginError::BadState);
    }
    let pending: PendingLogin = {
        let mut con = state.redis_pool.get().map_err(|err| LoginError::Spotify(err.to_string()))?;
        let raw: Option<String> = redis::cmd("GETDEL")
            .arg(format!("oauth:{oauth_state}"))
            .query(&mut con)
            .map_err(|err| LoginError::Spotify(err.to_string()))?;
        raw.and_then(|raw| serde_json::from_str(&raw).ok()).ok_or(LoginError::BadState)?
    };

    let tokens = spotify_user::exchange_code(&state.config, code, &pending.verifier)
        .await
        .map_err(|err| LoginError::Spotify(format!("{err:?}")))?;
    let profile = spotify_user::fetch_me(&tokens.access_token)
        .await
        .map_err(|err| LoginError::Spotify(format!("{err:?}")))?;
    let email = profile.email.as_deref().map(str::to_lowercase).ok_or(LoginError::NotInvited)?;
    let avatar = profile.images.first().map(|image| image.url.clone());

    let db = |err: diesel::result::Error| LoginError::Spotify(err.to_string());
    let mut connection = state.db_pool.get().map_err(|err| LoginError::Spotify(err.to_string()))?;
    let existing = diesel::sql_query(
        "SELECT id, status FROM users WHERE spotify_id = $1 OR email = $2 ORDER BY (spotify_id = $1) DESC NULLS LAST LIMIT 1",
    )
    .bind::<Text, _>(&profile.id)
    .bind::<Text, _>(&email)
    .get_result::<UserLookupRow>(&mut connection)
    .optional()
    .map_err(db)?
    .map(|row| ExistingUser { id: row.id, status: row.status });

    let user_id = match decide_login(existing.as_ref(), state.config.is_admin_email(&email)) {
        LoginDecision::Reject => return Err(LoginError::NotInvited),
        LoginDecision::CreateAdmin => diesel::sql_query(
            r#"
            INSERT INTO users (spotify_id, email, display_name, avatar_url, role, status, last_seen_at)
            VALUES ($1, $2, $3, $4, 'admin', 'active', now()) RETURNING id
            "#,
        )
        .bind::<Text, _>(&profile.id)
        .bind::<Text, _>(&email)
        .bind::<Nullable<Text>, _>(profile.display_name.as_deref())
        .bind::<Nullable<Text>, _>(avatar.as_deref())
        .get_result::<IdRow>(&mut connection)
        .map_err(db)?
        .id,
        LoginDecision::Activate { id, promote } => {
            diesel::sql_query(
                r#"
                UPDATE users SET spotify_id = $2, display_name = $3, avatar_url = $4, status = 'active',
                    role = CASE WHEN $5 THEN 'admin' ELSE role END, last_seen_at = now()
                WHERE id = $1
                "#,
            )
            .bind::<Integer, _>(id)
            .bind::<Text, _>(&profile.id)
            .bind::<Nullable<Text>, _>(profile.display_name.as_deref())
            .bind::<Nullable<Text>, _>(avatar.as_deref())
            .bind::<Bool, _>(promote)
            .execute(&mut connection)
            .map_err(db)?;
            id
        }
    };

    let cipher = TokenCipher::new(&state.config.token_key);
    spotify_user::store_tokens(&mut connection, &cipher, user_id, &tokens).map_err(db)?;
    let user_agent = request
        .headers()
        .get(actix_web::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let session = create_session(&mut connection, user_id, user_agent).map_err(db)?;
    tracing::info!(user_id, "User logged in with Spotify");
    Ok((session, pending.return_to))
}

#[get("/auth/callback")]
pub async fn callback(
    state: web::Data<AppState>,
    request: HttpRequest,
    query: web::Query<CallbackQuery>,
) -> HttpResponse {
    let secure = state.config.cookie_secure;
    let clear_oauth = cookie(OAUTH_COOKIE, String::new(), "/api/auth", CookieDuration::ZERO, secure);
    match complete_login(&state, &request, &query).await {
        Ok((session, return_to)) => redirect(&format!("/{return_to}"))
            .cookie(cookie(SESSION_COOKIE, session, "/", CookieDuration::days(SESSION_DAYS), secure))
            .cookie(clear_oauth)
            .finish(),
        Err(error) => {
            let code = match &error {
                LoginError::BadState => "state",
                LoginError::NotInvited => "not_invited",
                LoginError::Spotify(message) => {
                    tracing::warn!(%message, "Spotify login failed");
                    "spotify"
                }
            };
            redirect(&format!("/?login_error={code}")).cookie(clear_oauth).finish()
        }
    }
}

#[post("/auth/logout")]
pub async fn logout(state: web::Data<AppState>, request: HttpRequest) -> ApiResult<HttpResponse> {
    if let Some(session) = request.cookie(SESSION_COOKIE) {
        diesel::sql_query("DELETE FROM sessions WHERE token_hash = $1")
            .bind::<Text, _>(hash_token(session.value()))
            .execute(&mut state.db_pool.get()?)?;
    }
    Ok(HttpResponse::NoContent()
        .cookie(cookie(SESSION_COOKIE, String::new(), "/", CookieDuration::ZERO, state.config.cookie_secure))
        .finish())
}

// ---------------------------------------------------------------------------------------------
// Me and the invite list

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserResponse {
    id: i32,
    email: String,
    name: String,
    avatar_url: Option<String>,
    role: String,
    status: String,
    locale: Option<String>,
    /// Unix seconds.
    last_seen_at: Option<i64>,
}

#[derive(QueryableByName)]
struct UserRow {
    #[diesel(sql_type = Integer)]
    id: i32,
    #[diesel(sql_type = Text)]
    email: String,
    #[diesel(sql_type = Nullable<Text>)]
    display_name: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    avatar_url: Option<String>,
    #[diesel(sql_type = Text)]
    role: String,
    #[diesel(sql_type = Text)]
    status: String,
    #[diesel(sql_type = Nullable<Text>)]
    locale: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    last_seen_at: Option<i64>,
}

impl From<UserRow> for UserResponse {
    fn from(row: UserRow) -> Self {
        UserResponse {
            id: row.id,
            name: row.display_name.unwrap_or_else(|| row.email.clone()),
            email: row.email,
            avatar_url: row.avatar_url,
            role: row.role,
            status: row.status,
            locale: row.locale,
            last_seen_at: row.last_seen_at,
        }
    }
}

const USER_COLUMNS: &str = "id, email, display_name, avatar_url, role, status, locale, EXTRACT(EPOCH FROM last_seen_at)::bigint AS last_seen_at";

#[get("/auth/me")]
pub async fn me(state: web::Data<AppState>, principal: Principal) -> ApiResult<HttpResponse> {
    let Some(user_id) = principal.user_id() else {
        return Ok(HttpResponse::Ok().json(serde_json::json!({
            "id": 0, "email": "", "name": "service", "avatarUrl": null,
            "role": "admin", "status": "active", "locale": null, "lastSeenAt": null,
        })));
    };
    let row = diesel::sql_query(format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"))
        .bind::<Integer, _>(user_id)
        .get_result::<UserRow>(&mut state.db_pool.get()?)
        .optional()?
        .ok_or(ApiError::Unauthorized)?;
    Ok(HttpResponse::Ok().json(UserResponse::from(row)))
}

#[get("/admin/users")]
pub async fn list_users(state: web::Data<AppState>, _admin: RequireAdmin) -> ApiResult<HttpResponse> {
    let rows = diesel::sql_query(format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY (role = 'admin') DESC, status, lower(COALESCE(display_name, email))"
    ))
    .load::<UserRow>(&mut state.db_pool.get()?)?;
    Ok(HttpResponse::Ok().json(rows.into_iter().map(UserResponse::from).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
pub struct InviteBody {
    email: String,
    role: Option<String>,
}

fn looks_like_email(email: &str) -> bool {
    let mut parts = email.split('@');
    matches!((parts.next(), parts.next(), parts.next()), (Some(local), Some(domain), None)
        if !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'))
        && !email.chars().any(char::is_whitespace)
}

#[post("/admin/users")]
pub async fn invite_user(
    state: web::Data<AppState>,
    _admin: RequireAdmin,
    body: web::Json<InviteBody>,
) -> ApiResult<HttpResponse> {
    let email = body.email.trim().to_lowercase();
    if !looks_like_email(&email) {
        return Err(ApiError::BadRequest("That doesn't look like an email address".into()));
    }
    let role = match body.role.as_deref() {
        None => Role::Member,
        Some(value) => Role::parse(value).ok_or_else(|| ApiError::BadRequest("role must be admin or member".into()))?,
    };
    let mut connection = state.db_pool.get()?;
    let inserted = diesel::sql_query(format!(
        "INSERT INTO users (email, role, status) VALUES ($1, $2, 'invited') ON CONFLICT (email) DO NOTHING RETURNING {USER_COLUMNS}"
    ))
    .bind::<Text, _>(&email)
    .bind::<Text, _>(role.as_str())
    .get_result::<UserRow>(&mut connection)
    .optional()?
    .ok_or_else(|| ApiError::BadRequest(format!("{email} is already on the list")))?;
    Ok(HttpResponse::Created().json(UserResponse::from(inserted)))
}

#[derive(Deserialize)]
pub struct UpdateMeBody {
    locale: Option<String>,
}

/// The person's own preferences. Today only the UI language.
#[patch("/me")]
pub async fn update_me(
    state: web::Data<AppState>,
    principal: Principal,
    body: web::Json<UpdateMeBody>,
) -> ApiResult<HttpResponse> {
    let Some(user_id) = principal.user_id() else {
        return Err(ApiError::BadRequest("Service tokens have no preferences".into()));
    };
    let locale = match body.locale.as_deref() {
        Some(value @ ("es" | "en")) => value,
        _ => return Err(ApiError::BadRequest("locale must be es or en".into())),
    };
    let row = diesel::sql_query(format!("UPDATE users SET locale = $2 WHERE id = $1 RETURNING {USER_COLUMNS}"))
        .bind::<Integer, _>(user_id)
        .bind::<Text, _>(locale)
        .get_result::<UserRow>(&mut state.db_pool.get()?)
        .optional()?
        .ok_or(ApiError::Unauthorized)?;
    Ok(HttpResponse::Ok().json(UserResponse::from(row)))
}

#[derive(Deserialize)]
pub struct UpdateUserBody {
    role: Option<String>,
    status: Option<String>,
}

#[patch("/admin/users/{id}")]
pub async fn update_user(
    state: web::Data<AppState>,
    admin: RequireAdmin,
    path: web::Path<i32>,
    body: web::Json<UpdateUserBody>,
) -> ApiResult<HttpResponse> {
    let user_id = path.into_inner();
    let role = body
        .role
        .as_deref()
        .map(|value| Role::parse(value).ok_or_else(|| ApiError::BadRequest("role must be admin or member".into())))
        .transpose()?;
    let status = match body.status.as_deref() {
        None => None,
        Some(value @ ("active" | "disabled" | "invited")) => Some(value),
        Some(_) => return Err(ApiError::BadRequest("status must be active, invited or disabled".into())),
    };
    if admin.0.user_id() == Some(user_id) && (role == Some(Role::Member) || status == Some("disabled")) {
        return Err(ApiError::BadRequest("You can't remove your own admin access".into()));
    }

    let mut connection = state.db_pool.get()?;
    let row = diesel::sql_query(format!(
        "UPDATE users SET role = COALESCE($2, role), status = COALESCE($3, status) WHERE id = $1 RETURNING {USER_COLUMNS}"
    ))
    .bind::<Integer, _>(user_id)
    .bind::<Nullable<Text>, _>(role.map(Role::as_str))
    .bind::<Nullable<Text>, _>(status)
    .get_result::<UserRow>(&mut connection)
    .optional()?
    .ok_or(ApiError::NotFound("User not found"))?;
    if status == Some("disabled") {
        spotify_user::revoke_user_access(&mut connection, user_id)?;
        tracing::info!(user_id, "User disabled; sessions closed");
    }
    Ok(HttpResponse::Ok().json(UserResponse::from(row)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_unpadded_base64url_sha256() {
        // Expected value computed independently with Python's hashlib + urlsafe_b64encode.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn tokens_are_random_and_hash_is_stable() {
        let a = random_token(32);
        assert_ne!(a, random_token(32));
        assert_eq!(a.len(), 43);
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_eq!(hash_token("abc").len(), 64);
        assert_ne!(hash_token("abc"), "abc");
    }

    #[test]
    fn verifier_length_is_valid_for_pkce() {
        let verifier = random_token(48);
        assert!((43..=128).contains(&verifier.len()));
    }

    #[test]
    fn return_to_only_keeps_hash_routes() {
        assert_eq!(sanitize_return_to(Some("#/c/playlist/abc")), "#/c/playlist/abc");
        assert_eq!(sanitize_return_to(Some("https://evil.example")), "");
        assert_eq!(sanitize_return_to(Some("//evil.example")), "");
        assert_eq!(sanitize_return_to(Some("#a\r\nSet-Cookie: x")), "");
        assert_eq!(sanitize_return_to(None), "");
    }

    #[test]
    fn login_policy() {
        let invited = ExistingUser { id: 4, status: "invited".into() };
        let disabled = ExistingUser { id: 5, status: "disabled".into() };
        // Persona invitada entra por primera vez.
        assert_eq!(decide_login(Some(&invited), false), LoginDecision::Activate { id: 4, promote: false });
        // Persona no invitada.
        assert_eq!(decide_login(None, false), LoginDecision::Reject);
        // Admin inicial, sin invitación.
        assert_eq!(decide_login(None, true), LoginDecision::CreateAdmin);
        // Admin email on an existing member promotes it.
        assert_eq!(decide_login(Some(&invited), true), LoginDecision::Activate { id: 4, promote: true });
        // Disabled never gets in, not even as an admin email.
        assert_eq!(decide_login(Some(&disabled), true), LoginDecision::Reject);
    }

    #[test]
    fn admin_rights() {
        assert!(Principal::Service.is_admin());
        assert!(Principal::User { id: 1, role: Role::Admin }.is_admin());
        assert!(!Principal::User { id: 2, role: Role::Member }.is_admin());
        assert_eq!(Role::parse("owner"), None);
    }

    #[test]
    fn email_validation() {
        assert!(looks_like_email("lu@example.com"));
        assert!(!looks_like_email("lu@example"));
        assert!(!looks_like_email("lu example@x.com"));
        assert!(!looks_like_email("@example.com"));
        assert!(!looks_like_email("a@b@c.com"));
    }
}

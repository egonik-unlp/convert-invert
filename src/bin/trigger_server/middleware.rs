use std::net::IpAddr;

use actix_governor::{KeyExtractor, SimpleKeyExtractionError};
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::Method;
use actix_web::middleware::Next;
use actix_web::{Error, HttpMessage, web};
use subtle::ConstantTimeEq;

use crate::auth::{self, Principal};
use crate::errors::ApiError;
use crate::state::AppState;

/// Routes reachable without a session: health for orchestrators, and the login round trip.
const PUBLIC_PATHS: [&str; 4] = ["/api/health", "/api/auth/login", "/api/auth/callback", "/api/auth/logout"];

/// Authenticates every `/api` request and stores the resulting [`Principal`] in the request
/// extensions. A session cookie identifies a person; otherwise a valid `X-API-Key` (header, or
/// `?key=` on the two file-download routes) is accepted as the admin service token.
pub async fn require_auth(
    request: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    if PUBLIC_PATHS.contains(&request.path()) {
        return next.call(request).await;
    }

    let state = request
        .app_data::<web::Data<AppState>>()
        .ok_or_else(|| ApiError::Internal("AppState not configured".into()))?
        .clone();

    let from_session = match request.cookie(auth::SESSION_COOKIE) {
        Some(cookie) => {
            let mut connection = state.db_pool.get().map_err(ApiError::from)?;
            auth::session_principal(&mut connection, cookie.value()).map_err(ApiError::from)?
        }
        None => None,
    };

    let principal = match from_session {
        Some(principal) => {
            check_same_origin(&request, &state.config.allowed_origins)?;
            principal
        }
        None if api_key_matches(&request, &state.config.api_key) => Principal::Service,
        None => return Err(ApiError::Unauthorized.into()),
    };

    request.extensions_mut().insert(principal);
    next.call(request).await
}

fn api_key_matches(request: &ServiceRequest, api_key: &str) -> bool {
    // File downloads are plain browser navigations (`<a href>`), which can't carry a header, so
    // those two routes alone also accept the key as `?key=`.
    let query_key = matches!(request.path(), "/api/downloads/file" | "/api/downloads/zip")
        .then(|| {
            web::Query::<std::collections::HashMap<String, String>>::from_query(request.query_string())
                .ok()
                .and_then(|q| q.get("key").cloned())
        })
        .flatten();
    let provided = request
        .headers()
        .get("X-API-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or(query_key)
        .unwrap_or_default();
    constant_time_eq(api_key.as_bytes(), provided.as_bytes())
}

/// Constant-time compare that also covers a length mismatch without leaking the expected length.
fn constant_time_eq(expected: &[u8], presented: &[u8]) -> bool {
    if expected.len() == presented.len() {
        expected.ct_eq(presented).into()
    } else {
        let truncated = &expected[..expected.len().min(presented.len())];
        let _ = truncated.ct_eq(&presented[..truncated.len()]);
        false
    }
}

/// Cookie-authenticated writes must come from our own pages. `SameSite=Lax` already blocks most
/// cross-site POSTs; this closes the rest by checking `Origin` (or `Sec-Fetch-Site` when a browser
/// leaves `Origin` out).
fn check_same_origin(request: &ServiceRequest, allowed_origins: &[String]) -> Result<(), ApiError> {
    let header = |name: &str| request.headers().get(name).and_then(|value| value.to_str().ok());
    let host = header("X-Forwarded-Host").or_else(|| header("Host"));
    if is_same_origin_write(
        request.method(),
        header("Origin"),
        host,
        header("Sec-Fetch-Site"),
        allowed_origins,
    ) {
        Ok(())
    } else {
        Err(ApiError::Forbidden("Cross-site request blocked"))
    }
}

pub fn is_same_origin_write(
    method: &Method,
    origin: Option<&str>,
    host: Option<&str>,
    sec_fetch_site: Option<&str>,
    allowed_origins: &[String],
) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    match origin {
        Some(origin) => {
            allowed_origins.iter().any(|allowed| allowed == origin)
                || reqwest::Url::parse(origin)
                    .ok()
                    .and_then(|url| {
                        let authority = match url.port() {
                            Some(port) => format!("{}:{port}", url.host_str()?),
                            None => url.host_str()?.to_string(),
                        };
                        host.map(|host| host.eq_ignore_ascii_case(&authority))
                    })
                    .unwrap_or(false)
        }
        None => matches!(sec_fetch_site, Some("same-origin" | "none")),
    }
}

/// Rate-limit key: the client's IP. Behind the frontend's nginx every request arrives from the
/// nginx container, which would make the whole group share one bucket, so a proxy's
/// `X-Real-IP` is used when the peer is a private or loopback address. A LAN client could
/// forge the header, but that only moves it to another bucket; auth still applies.
#[derive(Clone)]
pub struct ClientIpKeyExtractor;

impl KeyExtractor for ClientIpKeyExtractor {
    type Key = IpAddr;
    type KeyExtractionError = SimpleKeyExtractionError<&'static str>;

    fn extract(&self, req: &ServiceRequest) -> Result<Self::Key, Self::KeyExtractionError> {
        let real_ip = req.headers().get("X-Real-IP").and_then(|value| value.to_str().ok());
        client_ip(req.peer_addr().map(|socket| socket.ip()), real_ip)
            .ok_or_else(|| SimpleKeyExtractionError::new("Could not extract client IP address from request"))
    }
}

pub fn client_ip(peer: Option<IpAddr>, real_ip: Option<&str>) -> Option<IpAddr> {
    let peer = peer?;
    let trusted = match peer {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    };
    let ip = trusted
        .then(|| real_ip.and_then(|value| value.trim().parse::<IpAddr>().ok()))
        .flatten()
        .unwrap_or(peer);
    // Same as PeerIpKeyExtractor: IPv6 clients usually hold a whole /56, so limit per prefix.
    Some(match ip {
        IpAddr::V6(ipv6) => {
            let mut octets = ipv6.octets();
            octets[7..16].fill(0);
            IpAddr::V6(octets.into())
        }
        v4 => v4,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        vec!["http://localhost:5173".to_string()]
    }

    #[test]
    fn reads_are_always_allowed() {
        assert!(is_same_origin_write(&Method::GET, Some("https://evil.example"), Some("app.example"), None, &allowed()));
    }

    #[test]
    fn same_origin_writes_pass() {
        assert!(is_same_origin_write(&Method::POST, Some("https://app.example"), Some("app.example"), None, &allowed()));
        assert!(is_same_origin_write(&Method::POST, Some("http://localhost:8080"), Some("localhost:8080"), None, &allowed()));
        assert!(is_same_origin_write(&Method::POST, Some("http://localhost:5173"), Some("api:3124"), None, &allowed()));
        assert!(is_same_origin_write(&Method::POST, None, Some("app.example"), Some("same-origin"), &allowed()));
    }

    #[test]
    fn cross_site_writes_are_blocked() {
        assert!(!is_same_origin_write(&Method::POST, Some("https://evil.example"), Some("app.example"), None, &allowed()));
        assert!(!is_same_origin_write(&Method::PATCH, Some("http://localhost:9999"), Some("localhost:8080"), None, &allowed()));
        assert!(!is_same_origin_write(&Method::POST, None, Some("app.example"), Some("cross-site"), &allowed()));
        assert!(!is_same_origin_write(&Method::POST, None, Some("app.example"), None, &allowed()));
        assert!(!is_same_origin_write(&Method::POST, Some("null"), Some("app.example"), None, &allowed()));
    }

    #[test]
    fn api_key_compare() {
        assert!(constant_time_eq(b"0123456789abcdef", b"0123456789abcdef"));
        assert!(!constant_time_eq(b"0123456789abcdef", b"0123456789abcdeX"));
        assert!(!constant_time_eq(b"0123456789abcdef", b"0123"));
        assert!(!constant_time_eq(b"0123456789abcdef", b""));
    }

    #[test]
    fn client_ip_trusts_real_ip_only_from_a_private_peer() {
        let nginx: IpAddr = "172.18.0.5".parse().unwrap();
        let public: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(client_ip(Some(nginx), Some("192.168.1.20")), Some("192.168.1.20".parse().unwrap()));
        assert_eq!(client_ip(Some(nginx), None), Some(nginx));
        assert_eq!(client_ip(Some(nginx), Some("garbage")), Some(nginx));
        assert_eq!(client_ip(Some(public), Some("192.168.1.20")), Some(public));
        assert_eq!(client_ip(None, Some("192.168.1.20")), None);
    }
}

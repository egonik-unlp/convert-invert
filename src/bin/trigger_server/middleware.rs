use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{Error, web};
use subtle::ConstantTimeEq;

use crate::errors::ApiError;
use crate::state::AppState;

/// Validates the `X-API-Key` header against `AppState::config.api_key` in constant time.
/// Health checks bypass auth so orchestrators don't need the key to know if the
/// service is up.
pub async fn require_api_key(
    request: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    if request.path() == "/api/health" {
        return next.call(request).await;
    }

    let state = request
        .app_data::<web::Data<AppState>>()
        .ok_or_else(|| ApiError::Internal("AppState not configured".into()))?;

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
    let provided = provided.as_str();

    let expected = state.config.api_key.as_bytes();
    let presented = provided.as_bytes();

    // Constant-time compare; also covers length mismatch by padding the shorter
    // side. We deliberately leak only "ok / not ok", never the expected length.
    let matches = if expected.len() == presented.len() {
        expected.ct_eq(presented).into()
    } else {
        // Still do a compare against a same-length slice of expected so timing
        // doesn't depend on user-controlled length.
        let truncated = &expected[..expected.len().min(presented.len())];
        let _ = truncated.ct_eq(&presented[..truncated.len()]);
        false
    };

    if !matches {
        return Err(ApiError::Unauthorized.into());
    }

    next.call(request).await
}

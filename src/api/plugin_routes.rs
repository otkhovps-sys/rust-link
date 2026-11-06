//! REST routes registered by plugins: unmatched requests are offered to them before the
//! 404 answer.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{json, Map, Value};

use super::{ApiError, AppState};
use crate::plugin::PluginError;

pub async fn fallback(
    State(ctx): State<AppState>,
    method: Method,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path();
    let Some((plugin, route, params)) = ctx.plugins.find_route(method.as_str(), path) else {
        return ApiError::not_found("Not Found").into_response();
    };
    let request = json!({
        "route": route,
        "method": method.as_str(),
        "path": path,
        "params": params,
        "query": query,
        "headers": header_object(&headers),
        "body": request_body(&headers, &body),
    });
    match ctx.plugins.call_route(&plugin, request).await {
        Ok(answer) => {
            let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::OK);
            let mut response = match &answer.body {
                Value::Null => status.into_response(),
                Value::String(text) => (
                    status,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    text.clone(),
                )
                    .into_response(),
                body => (status, axum::Json(body)).into_response(),
            };
            for (name, value) in &answer.headers {
                if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
                    response.headers_mut().insert(name, value);
                }
            }
            response
        }
        Err(e @ PluginError::Timeout { .. }) => {
            ApiError::new(StatusCode::GATEWAY_TIMEOUT, e.to_string()).into_response()
        }
        Err(e @ PluginError::NotRunning(_)) => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response()
        }
        Err(e) => ApiError::internal(e.to_string()).into_response(),
    }
}

/// Request headers for the plugin, without the node password.
fn header_object(headers: &HeaderMap) -> Map<String, Value> {
    headers
        .iter()
        .filter(|(name, _)| *name != header::AUTHORIZATION)
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), Value::String(v.to_string())))
        })
        .collect()
}

/// JSON when the body parses as JSON, text otherwise, `null` when empty.
fn request_body(headers: &HeaderMap, body: &Bytes) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    let json_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v.contains("json"));
    if json_type {
        if let Ok(value) = serde_json::from_slice(body) {
            return value;
        }
    }
    Value::String(String::from_utf8_lossy(body).into_owned())
}

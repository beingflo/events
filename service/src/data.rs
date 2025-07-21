use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, span};

use crate::{AppState, error::AppError};

#[tracing::instrument(skip_all)]
pub async fn upload_data(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(request): Json<Data>,
) -> Result<StatusCode, AppError> {
    let token = match headers.get("emitter") {
        Some(token) => token
            .to_str()
            .map_err(|_| AppError::Status(StatusCode::BAD_REQUEST))
            .unwrap(),
        None => {
            error!(message = "Missing token header");
            return Err(AppError::Status(StatusCode::BAD_REQUEST));
        }
    };

    if token != state.embedded_token {
        return Err(AppError::Status(StatusCode::UNAUTHORIZED));
    }

    let timestamp = if let Some(ts) = request.timestamp {
        ts
    } else {
        Timestamp::now().to_string()
    };

    let span = span!(
        tracing::Level::INFO,
        "data",
        bucket = request.bucket,
        timestamp,
        payload = serde_json::to_string(&request.payload)?,
    );
    let _span = span.enter();

    Ok(StatusCode::OK)
}

#[derive(Deserialize, Clone)]
pub struct Data {
    timestamp: Option<String>,
    bucket: String,
    payload: Value,
}

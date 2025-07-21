use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::span;

use crate::{AppState, error::AppError};

#[tracing::instrument(skip_all, fields( bucket = %bucket))]
pub async fn upload_gps_data(
    State(state): State<AppState>,
    Path((bucket, token)): Path<(String, String)>,
    Json(payload): Json<GPSData>,
) -> Result<Json<GPSUploadResponse>, AppError> {
    if token != state.gps_token {
        return Err(AppError::Status(StatusCode::UNAUTHORIZED));
    }
    for location in payload.locations {
        let span = span!(
            tracing::Level::INFO,
            "gps-location",
            bucket = bucket,
            location = serde_json::to_string(&location)?,
        );
        let _span = span.enter();
    }

    Ok(Json(GPSUploadResponse {
        result: "ok".into(),
    }))
}

#[derive(Debug, Serialize)]
pub struct GPSUploadResponse {
    result: String,
}

#[derive(Deserialize, Serialize, Clone)]
struct GPSGeometry {
    r#type: String,
    coordinates: [f64; 2],
}

#[derive(Deserialize, Serialize, Clone)]
struct GPSLocation {
    properties: Value,
    r#type: String,
    geometry: GPSGeometry,
}

#[derive(Deserialize, Clone)]
pub struct GPSData {
    locations: Vec<GPSLocation>,
}

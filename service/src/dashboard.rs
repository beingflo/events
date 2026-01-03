use std::env;

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::error;

use crate::error::AppError;

#[derive(Debug, Serialize, Deserialize)]
pub struct DashboardResponse {
    co2_values: Value,
    co2_latest: Value,
    hum_latest: Value,
}

#[tracing::instrument(skip_all)]
pub async fn get_dashboard_data(_headers: HeaderMap) -> Result<impl IntoResponse, AppError> {
    let client = reqwest::Client::new();

    let Some((_, ch_user)) = env::vars().find(|v| v.0.eq("CLICKHOUSE_USER")) else {
        error!("CLICKHOUSE_USER not in environment");
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    };
    let Some((_, ch_password)) = env::vars().find(|v| v.0.eq("CLICKHOUSE_PASSWORD")) else {
        error!("CLICKHOUSE_PASSWORD not in environment");
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    };

    let co2_query = "
    SELECT
        toStartOfInterval(parseDateTime64BestEffort(SpanAttributes['timestamp']), toIntervalMillisecond(10000)) as time,
        avg(JSONExtractFloat(SpanAttributes['payload'], 'co2')) as co2_ppm
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'co2')
        AND SpanName = 'data'
        AND (
            Timestamp >= now() - INTERVAL 3 HOUR
        )
    GROUP BY
        time
    ORDER BY
        time DESC;
    ";

    let co2_latest_query = "
    SELECT
        avg(JSONExtractFloat(SpanAttributes['payload'], 'co2')) as avg_co2
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'co2')
        AND SpanName = 'data'
        AND Timestamp >= now() - INTERVAL 2 MINUTE
    ";

    let hum_latest_query = "
    SELECT
        avg(JSONExtractFloat(SpanAttributes['payload'], 'humidity')) as humidity 
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'humidity')
        AND SpanName = 'data'
        AND SpanAttributes['bucket'] = 'humidity-laundry-room'
        AND Timestamp >= now() - INTERVAL 2 HOUR 
    ";

    let co2_response = client
        .post("http://localhost:8123/")
        .body(co2_query)
        .header("X-ClickHouse-Format", "JSON")
        .header("X-ClickHouse-User", &ch_user)
        .header("X-ClickHouse-Key", &ch_password)
        .send()
        .await
        .map_err(|e| {
            error!(message = "Failed to fetch data from Clickhouse", %e);
            AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    let co2_latest_response = client
        .post("http://localhost:8123/")
        .body(co2_latest_query)
        .header("X-ClickHouse-Format", "JSON")
        .header("X-ClickHouse-User", &ch_user)
        .header("X-ClickHouse-Key", &ch_password)
        .send()
        .await
        .map_err(|e| {
            error!(message = "Failed to fetch data from Clickhouse", %e);
            AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    let hum_latest_response = client
        .post("http://localhost:8123/")
        .body(hum_latest_query)
        .header("X-ClickHouse-Format", "JSON")
        .header("X-ClickHouse-User", &ch_user)
        .header("X-ClickHouse-Key", &ch_password)
        .send()
        .await
        .map_err(|e| {
            error!(message = "Failed to fetch data from Clickhouse", %e);
            AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    if !co2_response.status().is_success() {
        let error_text = co2_response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        error!(message = "Clickhouse responded with error", error_text);
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    if !co2_latest_response.status().is_success() {
        let error_text = co2_latest_response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        error!(message = "Clickhouse responded with error", error_text);
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    if !hum_latest_response.status().is_success() {
        let error_text = hum_latest_response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        error!(message = "Clickhouse responded with error", error_text);
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    let co2_data: Value = co2_response
        .json()
        .await
        .map_err(|_| AppError::Status(StatusCode::INTERNAL_SERVER_ERROR))?;

    let co2_latest_data: Value = co2_latest_response
        .json()
        .await
        .map_err(|_| AppError::Status(StatusCode::INTERNAL_SERVER_ERROR))?;

    let hum_latest_data: Value = hum_latest_response
        .json()
        .await
        .map_err(|_| AppError::Status(StatusCode::INTERNAL_SERVER_ERROR))?;

    // Return the data
    Ok(Json(DashboardResponse {
        co2_values: co2_data,
        co2_latest: co2_latest_data,
        hum_latest: hum_latest_data,
    }))
}

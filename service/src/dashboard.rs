use std::env;

use axum::{
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use plotters::prelude::*;
use serde_json::Value;
use tracing::error;

use crate::error::AppError;

#[tracing::instrument(skip_all)]
pub async fn get_dashboard(_headers: HeaderMap) -> Result<impl IntoResponse, AppError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| {
            error!(message = "Failed to create reqwest client", %e);
            AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    let Some((_, ch_host)) = env::vars().find(|v| v.0.eq("CLICKHOUSE_HOST")) else {
        error!("CLICKHOUSE_HOST not in environment");
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    };
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
            Timestamp >= now() - INTERVAL 3 DAYS
        )
    GROUP BY
        time
    ORDER BY
        time ASC;
    ";

    let co2_response = client
        .post(format!("http://{ch_host}:8123/"))
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

    if !co2_response.status().is_success() {
        let error_text = co2_response
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

    let data_points = parse_co2_data(&co2_data);
    if data_points.is_empty() {
        error!(message = "No CO2 data points returned from ClickHouse", raw_response = %co2_data);
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    let png_bytes = render_chart(&data_points).map_err(|e| {
        error!(message = "Failed to render chart", %e);
        AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/png".parse().unwrap());

    Ok((StatusCode::OK, headers, png_bytes))
}

fn parse_co2_data(json: &Value) -> Vec<(f64, f64)> {
    let Some(rows) = json.get("data").and_then(|d| d.as_array()) else {
        return vec![];
    };

    rows.iter()
        .filter_map(|row| {
            let co2_val = row.get("co2_ppm")?;
            let co2: f64 = co2_val
                .as_f64()
                .or_else(|| co2_val.as_str()?.parse().ok())?;
            Some(co2)
        })
        .enumerate()
        .map(|(i, co2)| (i as f64, co2))
        .collect()
}

fn render_chart(data: &[(f64, f64)]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const W: u32 = 792;
    const H: u32 = 272;

    let mut buf = vec![0u8; (W * H * 3) as usize];

    {
        let root = BitMapBackend::with_buffer(&mut buf, (W, H)).into_drawing_area();
        root.fill(&WHITE)?;

        let x_min = data.first().map(|d| d.0).unwrap_or(0.0);
        let x_max = data.last().map(|d| d.0).unwrap_or(1.0);
        let y_min = data.iter().map(|d| d.1).fold(f64::INFINITY, f64::min) - 50.0;
        let y_max = data.iter().map(|d| d.1).fold(f64::NEG_INFINITY, f64::max) + 50.0;

        let mut chart = ChartBuilder::on(&root)
            .margin(10)
            .x_label_area_size(20)
            .y_label_area_size(40)
            .build_cartesian_2d(x_min..x_max, y_min..y_max)?;

        chart
            .configure_mesh()
            .disable_x_mesh()
            .disable_y_mesh()
            .x_labels(0)
            .y_label_style(("sans-serif", 14).into_font().color(&BLACK))
            .draw()?;

        chart.draw_series(LineSeries::new(data.iter().copied(), &BLACK))?;

        root.present()?;
    }

    // Encode raw RGB buffer to PNG
    let mut png_bytes: Vec<u8> = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        image::ImageEncoder::write_image(encoder, &buf, W, H, image::ExtendedColorType::Rgb8)?;
    }

    Ok(png_bytes)
}

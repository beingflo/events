use std::env;

use axum::{
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use plotters::prelude::*;
use serde_json::Value;
use tracing::error;

use crate::error::AppError;

async fn ch_query(
    client: &reqwest::Client,
    url: &str,
    user: &str,
    password: &str,
    query: &str,
) -> Result<Value, AppError> {
    let response = client
        .post(url)
        .body(query.to_owned())
        .header("X-ClickHouse-Format", "JSON")
        .header("X-ClickHouse-User", user)
        .header("X-ClickHouse-Key", password)
        .send()
        .await
        .map_err(|e| {
            error!(message = "Failed to fetch data from Clickhouse", %e);
            AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;

    if !response.status().is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        error!(message = "Clickhouse responded with error", error_text);
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    response
        .json()
        .await
        .map_err(|_| AppError::Status(StatusCode::INTERNAL_SERVER_ERROR))
}

fn parse_scalar(json: &Value, field: &str) -> Option<f64> {
    let row = json.get("data")?.as_array()?.first()?;
    let val = row.get(field)?;
    val.as_f64().or_else(|| val.as_str()?.parse().ok())
}

#[tracing::instrument(skip_all)]
pub async fn get_dashboard(req_headers: HeaderMap) -> Result<impl IntoResponse, AppError> {
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

    let ch_url = format!("http://{ch_host}:8123/");
    let max_ts_subquery = "(SELECT max(Timestamp) FROM events.otel_traces WHERE SpanName = 'data')";

    let co2_query = format!("
    SELECT
        toStartOfInterval(parseDateTime64BestEffort(SpanAttributes['timestamp']), toIntervalMillisecond(10000)) as time,
        avg(JSONExtractFloat(SpanAttributes['payload'], 'co2')) as co2_ppm
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'co2')
        AND SpanName = 'data'
        AND Timestamp >= {max_ts_subquery} - INTERVAL 6 HOURS
    GROUP BY
        time
    ORDER BY
        time ASC;
    ");

    let temperature_query = format!("
    SELECT
        JSONExtractFloat(SpanAttributes['payload'], 'temperature') as temperature
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'temperature')
        AND SpanName = 'data'
        AND SpanAttributes['bucket'] = 'co2-sensor-living-room'
        AND Timestamp >= {max_ts_subquery} - INTERVAL 6 HOURS
    ORDER BY Timestamp DESC
    LIMIT 1;
    ");

    let pressure_query = format!("
    SELECT
        pow(
            (1 - ((0.0065 * 410) / (JSONExtractFloat(SpanAttributes['payload'], 'temperature') + 273.15 + 0.0065 * 400))),
            -5.257
        ) * JSONExtractFloat(SpanAttributes['payload'], 'absolute_pressure') / 100 as pressure
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'absolute_pressure')
        AND JSONHas(SpanAttributes['payload'], 'temperature')
        AND SpanName = 'data'
        AND SpanAttributes['bucket'] = 'brightness-barometer-living-room'
        AND Timestamp >= {max_ts_subquery} - INTERVAL 6 HOURS
    ORDER BY Timestamp DESC
    LIMIT 1;
    ");

    let humidity_query = format!("
    SELECT
        avg(JSONExtractFloat(SpanAttributes['payload'], 'humidity')) as humidity
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'humidity')
        AND SpanName = 'data'
        AND SpanAttributes['bucket'] = 'humidity-laundry-room'
        AND Timestamp >= {max_ts_subquery} - INTERVAL 1 HOUR;
    ");

    let co2_data = ch_query(&client, &ch_url, &ch_user, &ch_password, &co2_query).await?;
    let temperature_data = ch_query(&client, &ch_url, &ch_user, &ch_password, &temperature_query).await?;
    let pressure_data = ch_query(&client, &ch_url, &ch_user, &ch_password, &pressure_query).await?;
    let humidity_data = ch_query(&client, &ch_url, &ch_user, &ch_password, &humidity_query).await?;

    let _temperature: Option<f64> = parse_scalar(&temperature_data, "temperature");
    let _pressure: Option<f64> = parse_scalar(&pressure_data, "pressure");
    let _humidity: Option<f64> = parse_scalar(&humidity_data, "humidity");

    let data_points = parse_co2_data(&co2_data);
    if data_points.is_empty() {
        error!(message = "No CO2 data points returned from ClickHouse", raw_response = %co2_data);
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    }

    let wants_png = req_headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("image/png") || v.contains("image/*") || v.contains("*/*"))
        .unwrap_or(false);

    let rgb_buf = render_chart_rgb(&data_points).map_err(|e| {
        error!(message = "Failed to render chart", %e);
        AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;

    let refresh_seconds = env::var("DASHBOARD_REFRESH_SECONDS").unwrap_or_else(|_| "120".into());

    let mut headers = HeaderMap::new();
    headers.insert("X-Refresh-Seconds", refresh_seconds.parse().unwrap());

    if wants_png {
        let png_bytes = encode_png(&rgb_buf).map_err(|e| {
            error!(message = "Failed to encode PNG", %e);
            AppError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
        headers.insert(header::CONTENT_TYPE, "image/png".parse().unwrap());
        Ok((StatusCode::OK, headers, png_bytes))
    } else {
        let mono_buf = rgb_to_mono(&rgb_buf);
        headers.insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        Ok((StatusCode::OK, headers, mono_buf))
    }
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

const W: u32 = 792;
const H: u32 = 272;

fn render_chart_rgb(data: &[(f64, f64)]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut rgb_buf = vec![0u8; (W * H * 3) as usize];

    {
        let root = BitMapBackend::with_buffer(&mut rgb_buf, (W, H)).into_drawing_area();
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

    Ok(rgb_buf)
}

fn encode_png(rgb_buf: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut png_bytes: Vec<u8> = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
    image::ImageEncoder::write_image(encoder, rgb_buf, W, H, image::ExtendedColorType::Rgb8)?;
    Ok(png_bytes)
}

fn rgb_to_mono(rgb_buf: &[u8]) -> Vec<u8> {
    let row_bytes = (W as usize + 7) / 8;
    let mut mono_buf = vec![0u8; row_bytes * H as usize];

    for y in 0..H as usize {
        for x in 0..W as usize {
            let rgb_idx = (y * W as usize + x) * 3;
            let r = rgb_buf[rgb_idx] as u16;
            let g = rgb_buf[rgb_idx + 1] as u16;
            let b = rgb_buf[rgb_idx + 2] as u16;
            let luminance = (r + g + b) / 3;
            if luminance >= 128 {
                mono_buf[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }

    mono_buf
}

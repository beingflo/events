use std::env;

use axum::{
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};
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
    let Some((_, secret)) = env::vars().find(|v| v.0.eq("DASHBOARD_SECRET")) else {
        error!("DASHBOARD_SECRET not in environment");
        return Err(AppError::Status(StatusCode::INTERNAL_SERVER_ERROR));
    };

    let provided = req_headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == secret => {}
        _ => return Err(AppError::Status(StatusCode::UNAUTHORIZED)),
    }

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

    let temperature_query = format!(
        "
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
    "
    );

    let humidity_laundry_query = format!(
        "
    SELECT
        avg(JSONExtractFloat(SpanAttributes['payload'], 'humidity')) as humidity
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'humidity')
        AND SpanName = 'data'
        AND SpanAttributes['bucket'] = 'humidity-laundry-room'
        AND Timestamp >= {max_ts_subquery} - INTERVAL 1 HOUR;
    "
    );

    let humidity_living_query = format!(
        "
    SELECT
        avg(JSONExtractFloat(SpanAttributes['payload'], 'humidity')) as humidity_avg
    FROM
        events.otel_traces
    WHERE
        JSONHas(SpanAttributes['payload'], 'humidity')
        AND SpanName = 'data'
        AND SpanAttributes['bucket'] = 'co2-sensor-living-room'
        AND Timestamp >= {max_ts_subquery} - INTERVAL 1 HOUR;
    "
    );

    let co2_data = ch_query(&client, &ch_url, &ch_user, &ch_password, &co2_query).await?;
    let temperature_data =
        ch_query(&client, &ch_url, &ch_user, &ch_password, &temperature_query).await?;
    let humidity_laundry_data = ch_query(
        &client,
        &ch_url,
        &ch_user,
        &ch_password,
        &humidity_laundry_query,
    )
    .await?;
    let humidity_living_data = ch_query(
        &client,
        &ch_url,
        &ch_user,
        &ch_password,
        &humidity_living_query,
    )
    .await?;

    let temperature: Option<f64> = parse_scalar(&temperature_data, "temperature");
    let humidity_laundry: Option<f64> = parse_scalar(&humidity_laundry_data, "humidity");
    let humidity_living: Option<f64> = parse_scalar(&humidity_living_data, "humidity_avg");

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

    let latest_co2: Option<f64> = data_points.last().map(|(_, co2)| *co2);

    let rgb_buf = render_chart_rgb(
        &data_points,
        temperature,
        latest_co2,
        humidity_laundry,
        humidity_living,
    )
    .map_err(|e| {
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

/// Returns (time_str, co2_ppm) pairs
fn parse_co2_data(json: &Value) -> Vec<(String, f64)> {
    let Some(rows) = json.get("data").and_then(|d| d.as_array()) else {
        return vec![];
    };

    rows.iter()
        .filter_map(|row| {
            let time_str = row.get("time")?.as_str()?.to_string();
            let co2_val = row.get("co2_ppm")?;
            let co2: f64 = co2_val
                .as_f64()
                .or_else(|| co2_val.as_str()?.parse().ok())?;
            Some((time_str, co2))
        })
        .collect()
}

const W: u32 = 792;
const H: u32 = 272;

fn render_chart_rgb(
    data: &[(String, f64)],
    temperature: Option<f64>,
    co2_latest: Option<f64>,
    humidity_laundry: Option<f64>,
    humidity_living: Option<f64>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const SCALE: u32 = 2;
    let sw = W * SCALE;
    let sh = H * SCALE;
    let mut hi_buf = vec![0u8; (sw * sh * 3) as usize];

    // Build indexed data for plotting, and a lookup from index to time string
    let indexed: Vec<(f64, f64)> = data
        .iter()
        .enumerate()
        .map(|(i, (_, co2))| (i as f64, *co2))
        .collect();
    let n = data.len();

    {
        let root = BitMapBackend::with_buffer(&mut hi_buf, (sw, sh)).into_drawing_area();
        root.fill(&WHITE)?;

        let chart_width = (sw * 4 / 5) as i32;
        let (chart_area, info_area) = root.split_horizontally(chart_width);

        let x_min = 0.0f64;
        let x_max = (n.saturating_sub(1)) as f64;
        let y_min = data.iter().map(|d| d.1).fold(f64::INFINITY, f64::min) - 50.0;
        let y_max = data.iter().map(|d| d.1).fold(f64::NEG_INFINITY, f64::max) + 50.0;

        let mut chart = ChartBuilder::on(&chart_area)
            .margin(10 * SCALE as i32)
            .x_label_area_size(24 * SCALE)
            .y_label_area_size(48 * SCALE)
            .build_cartesian_2d(x_min..x_max, y_min..y_max)?;

        // Extract HH:MM from timestamp strings, converted to Zurich timezone
        let zurich = jiff::tz::TimeZone::get("Europe/Zurich").expect("valid tz");
        let time_labels: Vec<String> = data
            .iter()
            .map(|(ts, _)| {
                // Timestamp format: "2026-04-02 19:42:10.000" (UTC)
                // Parse as UTC, convert to Zurich, format as HH:MM
                let formatted = ts
                    .parse::<jiff::civil::DateTime>()
                    .ok()
                    .and_then(|dt| dt.to_zoned(jiff::tz::TimeZone::UTC).ok())
                    .map(|zdt| zdt.with_time_zone(zurich.clone()))
                    .map(|zdt| format!("{:02}:{:02}", zdt.hour(), zdt.minute()));
                formatted.unwrap_or_default()
            })
            .collect();

        chart
            .configure_mesh()
            .disable_x_mesh()
            .disable_y_mesh()
            .x_labels(10)
            .x_label_formatter(&|x| {
                let idx = *x as usize;
                time_labels.get(idx).cloned().unwrap_or_default()
            })
            .x_label_style(
                FontDesc::new(FontFamily::SansSerif, 16.0 * SCALE as f64, FontStyle::Bold)
                    .color(&BLACK),
            )
            .y_label_style(
                FontDesc::new(FontFamily::SansSerif, 16.0 * SCALE as f64, FontStyle::Bold)
                    .color(&BLACK),
            )
            .draw()?;

        chart.draw_series(LineSeries::new(
            indexed.into_iter(),
            ShapeStyle::from(&BLACK).stroke_width(2 * SCALE),
        ))?;

        let chart_dim = chart_area.dim_in_pixel();
        chart_area.draw_text(
            "CO2 Living Room [ppm]",
            &FontDesc::new(FontFamily::SansSerif, 14.0 * SCALE as f64, FontStyle::Bold)
                .color(&BLACK)
                .pos(Pos::new(HPos::Right, VPos::Top)),
            (chart_dim.0 as i32 - 12 * SCALE as i32, 12 * SCALE as i32),
        )?;

        // Vertical separator between chart and info panel
        let info_dim = info_area.dim_in_pixel();
        let info_w = info_dim.0 as i32;
        let info_h = info_dim.1 as i32;
        let cell_h = info_h / 4;

        let sep_style = ShapeStyle::from(&BLACK).stroke_width(SCALE);
        info_area.draw(&PathElement::new(vec![(0, 0), (0, info_h)], sep_style))?;
        for row in 1..4 {
            info_area.draw(&PathElement::new(
                vec![(0, cell_h * row), (info_w, cell_h * row)],
                sep_style,
            ))?;
        }

        let label_style =
            FontDesc::new(FontFamily::SansSerif, 16.0 * SCALE as f64, FontStyle::Bold)
                .color(&BLACK);
        let value_style =
            FontDesc::new(FontFamily::SansSerif, 24.0 * SCALE as f64, FontStyle::Bold)
                .color(&BLACK);

        let scalars: [(&str, Option<f64>, &str); 4] = [
            ("Temperature", temperature, "C"),
            ("CO2", co2_latest, "ppm"),
            ("Hum. Laundry", humidity_laundry, "%"),
            ("Hum. Living", humidity_living, "%"),
        ];

        for (i, (label, val, unit)) in scalars.iter().enumerate() {
            let y_offset = cell_h * i as i32;
            let center_x = info_w / 2;

            info_area.draw_text(
                label,
                &label_style.pos(Pos::new(HPos::Center, VPos::Top)),
                (center_x, y_offset + 8 * SCALE as i32),
            )?;

            let value_text = match val {
                Some(v) => format!("{v:.1} {unit}"),
                None => "--".to_string(),
            };
            info_area.draw_text(
                &value_text,
                &value_style.pos(Pos::new(HPos::Center, VPos::Center)),
                (center_x, y_offset + cell_h / 2 + 8 * SCALE as i32),
            )?;
        }

        root.present()?;
    }

    // Downscale from 2x to 1x with averaging
    let mut rgb_buf = vec![0u8; (W * H * 3) as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            for c in 0..3 {
                let mut sum = 0u32;
                for dy in 0..SCALE as usize {
                    for dx in 0..SCALE as usize {
                        let hi_x = x * SCALE as usize + dx;
                        let hi_y = y * SCALE as usize + dy;
                        sum += hi_buf[(hi_y * sw as usize + hi_x) * 3 + c] as u32;
                    }
                }
                rgb_buf[(y * W as usize + x) * 3 + c] = (sum / (SCALE * SCALE) as u32) as u8;
            }
        }
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

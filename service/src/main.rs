use std::{env, process::abort, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
    routing::post,
};
use error::AppError;
use opentelemetry::{global, trace::TracerProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tokio::signal;
use tower_http::{classify::ServerErrorsFailureClass, trace::TraceLayer};
use tracing::{Span, error, info, warn};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

use crate::{data::upload_data, gps::upload_gps_data};

mod data;
mod error;
mod gps;

#[derive(Clone)]
struct AppState {
    gps_token: String,
    embedded_token: String,
}

#[tokio::main]
pub async fn main() -> Result<(), AppError> {
    match dotenvy::dotenv() {
        Ok(_) => info!("Loaded .env file"),
        Err(_) => warn!("Failed to load .env file"),
    };

    let Some((_, gps_token)) = env::vars().find(|v| v.0.eq("GPS_PUSH_TOKEN")) else {
        error!("GPS push token not in environment");
        abort();
    };
    let Some((_, embedded_token)) = env::vars().find(|v| v.0.eq("EMBEDDED_TOKEN")) else {
        error!("Embedded token not in environment");
        abort();
    };

    let tracer = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(tracer)
        .build();

    global::set_tracer_provider(provider.clone());

    // Set up tracing with both console output and OpenTelemetry
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        )
        .with(
            OpenTelemetryLayer::new(provider.tracer("events-service"))
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        )
        .init();

    let app = Router::new()
        .route("/api/data", post(upload_data))
        .route("/api/gps/:bucket/:token", post(upload_gps_data))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|_request: &Request<Body>| {
                    let request_id = Uuid::new_v4().to_string();
                    tracing::info_span!("http-request", %request_id)
                })
                .on_request(|request: &Request<Body>, _span: &Span| {
                    info!(
                        message = "request",
                        request = request.method().as_str(),
                        uri = request.uri().path().to_string(),
                        referrer = request
                            .headers()
                            .get("referer")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or(""),
                        user_agent = request
                            .headers()
                            .get("user-agent")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                    )
                })
                .on_response(
                    |response: &Response<Body>, latency: Duration, _span: &Span| {
                        info!(
                            message = "response_status",
                            status = response.status().as_u16(),
                            latency = latency.as_nanos()
                        )
                    },
                )
                .on_failure(
                    |error: ServerErrorsFailureClass, _latency: Duration, _span: &Span| {
                        error!(message = "error", error = error.to_string())
                    },
                ),
        )
        .with_state(AppState {
            gps_token: gps_token,
            embedded_token: embedded_token,
        });

    let port = 3000;
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .map_err(|e| {
            error!(message = "Failed to create TCP listener", error=%e);
            AppError::Status(StatusCode::SERVICE_UNAVAILABLE)
        })?;

    info!(message = "Starting server", port);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| {
            error!(message = "Failed to start server", error=%e);
            AppError::Status(StatusCode::SERVICE_UNAVAILABLE)
        })?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {
            info!("Ctrl+C received, shutting down")
        },
        _ = terminate => {
            info!("SIGTERM received, shutting down")
        },
    }
}

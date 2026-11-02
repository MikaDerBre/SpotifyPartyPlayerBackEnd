mod auth;
mod config;

use auth::{
    get_auth_status, get_current_user, logout, spotify_callback, spotify_login_redirect,
    spotify_login_url, AppState,
};
use axum::{
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use config::AppConfig;
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    message: &'static str,
    spotify_configured: bool,
}

async fn root() -> &'static str {
    "Spotify Party Player API Server"
}

async fn health_check(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        message: "Axum backend is up and running!",
        spotify_configured: state.config.is_spotify_configured(),
    })
}

#[tokio::main]
async fn main() {
    // Load .env file if available
    let _ = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "backend=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = AppConfig::from_env();
    tracing::info!("Spotify Client configured: {}", config.is_spotify_configured());
    tracing::info!("Frontend URL: {}", config.frontend_url);
    tracing::info!("Spotify Redirect URI: {}", config.spotify_redirect_uri);

    let state = AppState {
        config: config.clone(),
        http_client: reqwest::Client::new(),
        sessions: Arc::new(RwLock::new(HashMap::new())),
        oauth_states: Arc::new(RwLock::new(HashMap::new())),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(root))
        .route("/api/health", get(health_check))
        .route("/api/auth/status", get(get_auth_status))
        .route("/api/auth/spotify/login", get(spotify_login_redirect))
        .route("/api/auth/spotify/url", get(spotify_login_url))
        .route("/api/auth/spotify/callback", get(spotify_callback))
        .route("/auth/callback", get(spotify_callback))
        .route("/api/auth/me", get(get_current_user))
        .route("/api/auth/logout", post(logout))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("Server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind TCP listener");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}

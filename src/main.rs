mod auth;
mod config;
mod player_sync;
mod room;
mod spotify;

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
use room::{
    add_to_queue, create_room, delete_room, get_player_diagnostics, get_room, join_room,
    leave_room, link_host, remove_from_queue, skip_current_track, vote_track,
};
use serde::Serialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    message: &'static str,
    spotify_configured: bool,
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
        rooms: Arc::new(RwLock::new(HashMap::new())),
    };

    // Start background worker for player status sync & auto-enqueue transition
    player_sync::start_sync_loop(state.clone());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let static_dir = "static";
    let serve_dir = ServeDir::new(static_dir)
        .not_found_service(ServeFile::new(format!("{}/index.html", static_dir)));

    let app = Router::new()
        .route("/api/health", get(health_check))
        // Auth routes
        .route("/api/auth/status", get(get_auth_status))
        .route("/api/auth/spotify/login", get(spotify_login_redirect))
        .route("/api/auth/spotify/url", get(spotify_login_url))
        .route("/api/auth/spotify/callback", get(spotify_callback))
        .route("/auth/callback", get(spotify_callback))
        .route("/api/auth/me", get(get_current_user))
        .route("/api/auth/logout", post(logout))
        // Room routes
        .route("/api/rooms", post(create_room))
        .route("/api/rooms/{code}", get(get_room).delete(delete_room))
        .route("/api/rooms/{code}/join", post(join_room))
        .route("/api/rooms/{code}/leave", post(leave_room))
        // Room Queue & Voting
        .route("/api/rooms/{code}/queue", post(add_to_queue))
        .route(
            "/api/rooms/{code}/queue/{track_id}/vote",
            post(vote_track),
        )
        .route(
            "/api/rooms/{code}/queue/{track_id}",
            axum::routing::delete(remove_from_queue),
        )
        // Playback control (Host only)
        .route("/api/rooms/{code}/playback/skip", post(skip_current_track))
        .route("/api/rooms/{code}/link-host", post(link_host))
        .route("/api/rooms/{code}/player-diagnostics", get(get_player_diagnostics))
        // Spotify search
        .route("/api/spotify/search", get(spotify::search_tracks))
        .fallback_service(serve_dir)
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

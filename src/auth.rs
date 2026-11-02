use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Redirect,
    Json,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::config::AppConfig;

const SPOTIFY_SCOPES: &str = "user-read-private user-read-email user-read-playback-state user-modify-playback-state user-read-currently-playing streaming playlist-read-private playlist-modify-public playlist-modify-private";

use crate::room::Room;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub http_client: reqwest::Client,
    pub sessions: Arc<RwLock<HashMap<String, UserSession>>>,
    pub oauth_states: Arc<RwLock<HashMap<String, Instant>>>,
    pub rooms: Arc<RwLock<HashMap<String, Room>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpotifyImage {
    pub url: String,
    pub height: Option<u32>,
    pub width: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpotifyUserProfile {
    pub id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub country: Option<String>,
    pub product: Option<String>,
    pub uri: Option<String>,
    #[serde(default)]
    pub images: Vec<SpotifyImage>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct UserSession {
    pub session_id: String,
    pub user: SpotifyUserProfile,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Instant,
}

#[derive(Deserialize)]
struct SpotifyTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct AuthStatusResponse {
    pub configured: bool,
    pub authenticated: bool,
    pub user: Option<SpotifyUserProfile>,
}

pub async fn get_auth_url(app_state: &AppState) -> (String, String) {
    let state = Uuid::new_v4().to_string();
    {
        let mut states = app_state.oauth_states.write().await;
        // Clean up states older than 10 minutes
        states.retain(|_, time| time.elapsed() < Duration::from_secs(600));
        states.insert(state.clone(), Instant::now());
    }

    let auth_url = format!(
        "https://accounts.spotify.com/authorize?response_type=code&client_id={}&scope={}&redirect_uri={}&state={}",
        urlencoding(&app_state.config.spotify_client_id),
        urlencoding(SPOTIFY_SCOPES),
        urlencoding(&app_state.config.spotify_redirect_uri),
        urlencoding(&state),
    );

    (state, auth_url)
}

pub fn urlencoding(input: &str) -> String {
    url::form_urlencoded::byte_serialize(input.as_bytes()).collect()
}

pub async fn spotify_login_url(State(state): State<AppState>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.config.is_spotify_configured() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Spotify credentials not configured in backend/.env"
            })),
        ));
    }

    let (_, auth_url) = get_auth_url(&state).await;
    Ok(Json(serde_json::json!({ "url": auth_url })))
}

pub async fn spotify_login_redirect(State(state): State<AppState>) -> Result<Redirect, (StatusCode, String)> {
    if !state.config.is_spotify_configured() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Spotify credentials not configured. Please set SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET in backend/.env".into(),
        ));
    }

    let (_, auth_url) = get_auth_url(&state).await;
    Ok(Redirect::temporary(&auth_url))
}

pub async fn spotify_callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackQuery>,
) -> Result<Redirect, (StatusCode, String)> {
    let frontend_url = state.config.frontend_url.clone();

    if let Some(err) = params.error {
        tracing::warn!("Spotify auth error: {}", err);
        let redirect_url = format!("{}/#/callback?error={}", frontend_url, urlencoding(&err));
        return Ok(Redirect::temporary(&redirect_url));
    }

    let code = match params.code {
        Some(c) => c,
        None => {
            let redirect_url = format!("{}/#/callback?error=missing_code", frontend_url);
            return Ok(Redirect::temporary(&redirect_url));
        }
    };

    let oauth_state = match params.state {
        Some(s) => s,
        None => {
            let redirect_url = format!("{}/#/callback?error=missing_state", frontend_url);
            return Ok(Redirect::temporary(&redirect_url));
        }
    };

    // Verify state
    {
        let mut states = state.oauth_states.write().await;
        if states.remove(&oauth_state).is_none() {
            tracing::warn!("Invalid or expired OAuth state: {}", oauth_state);
            let redirect_url = format!("{}/#/callback?error=invalid_state", frontend_url);
            return Ok(Redirect::temporary(&redirect_url));
        }
    }

    // Exchange authorization code for access token
    let credentials = format!(
        "{}:{}",
        state.config.spotify_client_id, state.config.spotify_client_secret
    );
    let basic_auth = format!("Basic {}", BASE64.encode(credentials.as_bytes()));

    let form_params = [
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", &state.config.spotify_redirect_uri),
    ];

    let token_res = state
        .http_client
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", basic_auth)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&form_params)
        .send()
        .await;

    let token_data: SpotifyTokenResponse = match token_res {
        Ok(res) if res.status().is_success() => match res.json().await {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("Failed to parse Spotify token response: {}", e);
                let redirect_url = format!("{}/#/callback?error=token_parse_error", frontend_url);
                return Ok(Redirect::temporary(&redirect_url));
            }
        },
        Ok(res) => {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            tracing::error!("Spotify token exchange failed with status {}: {}", status, body);
            let redirect_url = format!("{}/#/callback?error=token_exchange_failed", frontend_url);
            return Ok(Redirect::temporary(&redirect_url));
        }
        Err(e) => {
            tracing::error!("Network error while contacting Spotify token endpoint: {}", e);
            let redirect_url = format!("{}/#/callback?error=network_error", frontend_url);
            return Ok(Redirect::temporary(&redirect_url));
        }
    };

    // Fetch user profile from Spotify
    let user_res = state
        .http_client
        .get("https://api.spotify.com/v1/me")
        .bearer_auth(&token_data.access_token)
        .send()
        .await;

    let user_profile: SpotifyUserProfile = match user_res {
        Ok(res) if res.status().is_success() => match res.json().await {
            Ok(profile) => profile,
            Err(e) => {
                tracing::error!("Failed to parse Spotify user profile: {}", e);
                let redirect_url = format!("{}/#/callback?error=profile_parse_error", frontend_url);
                return Ok(Redirect::temporary(&redirect_url));
            }
        },
        Ok(res) => {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            tracing::error!("Spotify /v1/me failed with status {}: {}", status, body);
            let redirect_url = format!("{}/#/callback?error=profile_fetch_failed", frontend_url);
            return Ok(Redirect::temporary(&redirect_url));
        }
        Err(e) => {
            tracing::error!("Network error while fetching Spotify profile: {}", e);
            let redirect_url = format!("{}/#/callback?error=network_error", frontend_url);
            return Ok(Redirect::temporary(&redirect_url));
        }
    };

    // Create session
    let session_id = Uuid::new_v4().to_string();
    let expires_at = Instant::now() + Duration::from_secs(token_data.expires_in.saturating_sub(60));

    {
        let mut sessions = state.sessions.write().await;
        sessions.insert(
            session_id.clone(),
            UserSession {
                session_id: session_id.clone(),
                user: user_profile,
                access_token: token_data.access_token,
                refresh_token: token_data.refresh_token,
                expires_at,
            },
        );
    }

    // Redirect back to frontend SPA with token
    let redirect_url = format!("{}/#/callback?token={}", frontend_url, session_id);
    tracing::info!("User logged in successfully. Redirecting to {}", redirect_url);
    Ok(Redirect::temporary(&redirect_url))
}

pub fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth_val) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth_val.strip_prefix("Bearer ") {
            return Some(token.trim().to_string());
        }
    }
    None
}

pub async fn get_current_user(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SpotifyUserProfile>, (StatusCode, Json<serde_json::Value>)> {
    let token = extract_session_token(&headers).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Missing or invalid Authorization header" })),
        )
    })?;

    let sessions = state.sessions.read().await;
    let session = sessions.get(&token).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Invalid or expired session" })),
        )
    })?;

    Ok(Json(session.user.clone()))
}

pub async fn get_auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<AuthStatusResponse> {
    let configured = state.config.is_spotify_configured();
    let mut authenticated = false;
    let mut user = None;

    if let Some(token) = extract_session_token(&headers) {
        let sessions = state.sessions.read().await;
        if let Some(s) = sessions.get(&token) {
            authenticated = true;
            user = Some(s.user.clone());
        }
    }

    Json(AuthStatusResponse {
        configured,
        authenticated,
        user,
    })
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(token) = extract_session_token(&headers) {
        let mut sessions = state.sessions.write().await;
        sessions.remove(&token);
    }

    Ok(Json(serde_json::json!({ "success": true })))
}

pub async fn ensure_host_token(state: &AppState, host_id: &str) -> Option<String> {
    let (mut session_to_refresh, session_key) = {
        let sessions = state.sessions.read().await;
        let found = sessions.iter().find(|(_, s)| s.user.id == host_id)?;
        if found.1.expires_at > Instant::now() + Duration::from_secs(60) {
            return Some(found.1.access_token.clone());
        }
        (found.1.clone(), found.0.clone())
    };

    let refresh_token = session_to_refresh.refresh_token.as_ref()?;
    let credentials = format!(
        "{}:{}",
        state.config.spotify_client_id, state.config.spotify_client_secret
    );
    let basic_auth = format!("Basic {}", BASE64.encode(credentials.as_bytes()));

    let form_params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
    ];

    let res = state
        .http_client
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", basic_auth)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&form_params)
        .send()
        .await
        .ok()?;

    if res.status().is_success() {
        if let Ok(data) = res.json::<SpotifyTokenResponse>().await {
            let new_access_token = data.access_token.clone();
            session_to_refresh.access_token = data.access_token;
            if let Some(new_rt) = data.refresh_token {
                session_to_refresh.refresh_token = Some(new_rt);
            }
            session_to_refresh.expires_at =
                Instant::now() + Duration::from_secs(data.expires_in.saturating_sub(60));

            let mut sessions = state.sessions.write().await;
            sessions.insert(session_key, session_to_refresh);
            return Some(new_access_token);
        }
    }

    Some(session_to_refresh.access_token)
}

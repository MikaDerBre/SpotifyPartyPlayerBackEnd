use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::auth::{extract_session_token, AppState};

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub room_code: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackItem {
    pub id: String,
    pub name: String,
    pub uri: String,
    pub artists: Vec<String>,
    pub artist_display: String,
    pub album_name: String,
    pub album_art: Option<String>,
    pub duration_ms: u64,
    pub explicit: bool,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub tracks: Vec<TrackItem>,
}

// Internal structures to parse Spotify's raw search response
#[derive(Deserialize)]
struct SpotifyArtist {
    name: String,
}

#[derive(Deserialize)]
struct SpotifyAlbumImage {
    url: String,
}

#[derive(Deserialize)]
struct SpotifyAlbum {
    name: String,
    #[serde(default)]
    images: Vec<SpotifyAlbumImage>,
}

#[derive(Deserialize)]
struct SpotifyRawTrack {
    id: String,
    name: String,
    uri: String,
    #[serde(default)]
    artists: Vec<SpotifyArtist>,
    album: SpotifyAlbum,
    duration_ms: u64,
    #[serde(default)]
    explicit: bool,
}

#[derive(Deserialize)]
struct SpotifyTracksContainer {
    #[serde(default)]
    items: Vec<SpotifyRawTrack>,
}

#[derive(Deserialize)]
struct SpotifyRawSearchResponse {
    tracks: Option<SpotifyTracksContainer>,
}

#[derive(Deserialize)]
struct ClientCredentialsTokenResponse {
    access_token: String,
}

async fn get_access_token_for_search(
    state: &AppState,
    headers: &HeaderMap,
    room_code: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    // 1. Check if user is logged in
    if let Some(token) = extract_session_token(headers) {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&token) {
            return Ok(session.access_token.clone());
        }
    }

    // 2. Check if a room code was provided and use the host's token
    if let Some(code) = room_code {
        let rooms = state.rooms.read().await;
        if let Some(room) = rooms.get(&code.to_uppercase()) {
            let host_id = &room.host_id;
            let sessions = state.sessions.read().await;
            for s in sessions.values() {
                if &s.user.id == host_id {
                    return Ok(s.access_token.clone());
                }
            }
        }
    }

    // 3. Any active logged-in host/session token
    {
        let sessions = state.sessions.read().await;
        if let Some(first_session) = sessions.values().next() {
            if first_session.expires_at > Instant::now() {
                return Ok(first_session.access_token.clone());
            }
        }
    }

    // 4. Fallback to Client Credentials flow (App Token)
    get_client_credentials_token(state).await
}

async fn get_client_credentials_token(state: &AppState) -> Result<String, (StatusCode, String)> {
    if !state.config.is_spotify_configured() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Spotify credentials not configured in backend/.env".into(),
        ));
    }

    let credentials = format!(
        "{}:{}",
        state.config.spotify_client_id, state.config.spotify_client_secret
    );
    let basic_auth = format!("Basic {}", BASE64.encode(credentials.as_bytes()));

    let res = state
        .http_client
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", basic_auth)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Token request failed: {}", e)))?;

    if !res.status().is_success() {
        let err_text = res.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Spotify token request failed: {}", err_text),
        ));
    }

    let token_data: ClientCredentialsTokenResponse = res
        .json()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to parse token: {}", e)))?;

    Ok(token_data.access_token)
}

// GET /api/spotify/search?q={query}&room_code={optional}&limit={optional}
pub async fn search_tracks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<serde_json::Value>)> {
    let query_str = params.q.trim();
    if query_str.is_empty() {
        return Ok(Json(SearchResponse {
            query: String::new(),
            tracks: Vec::new(),
        }));
    }

    let access_token = get_access_token_for_search(
        &state,
        &headers,
        params.room_code.as_deref(),
    )
    .await
    .map_err(|(status, msg)| (status, Json(serde_json::json!({ "error": msg }))))?;

    let limit = params.limit.unwrap_or(10).clamp(1, 10);

    let search_url = format!(
        "https://api.spotify.com/v1/search?q={}&type=track&limit={}",
        url::form_urlencoded::byte_serialize(query_str.as_bytes()).collect::<String>(),
        limit
    );

    let mut spotify_res = state
        .http_client
        .get(&search_url)
        .bearer_auth(&access_token)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": format!("Spotify API error: {}", e) })),
            )
        })?;

    // If first attempt failed (e.g. dev mode token restriction), retry with client credentials
    if !spotify_res.status().is_success() {
        tracing::warn!("Spotify search with initial token failed ({}), attempting client credentials fallback...", spotify_res.status());
        if let Ok(cc_token) = get_client_credentials_token(&state).await {
            if let Ok(retry_res) = state.http_client.get(&search_url).bearer_auth(&cc_token).send().await {
                if retry_res.status().is_success() {
                    spotify_res = retry_res;
                }
            }
        }
    }

    if !spotify_res.status().is_success() {
        let status = spotify_res.status();
        let body = spotify_res.text().await.unwrap_or_default();
        tracing::error!("Spotify search error ({}): {}", status, body);
        return Err((
            status,
            Json(serde_json::json!({ "error": "Spotify Suche fehlgeschlagen", "details": body })),
        ));
    }

    let raw: SpotifyRawSearchResponse = spotify_res.json().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("Failed to parse tracks: {}", e) })),
        )
    })?;

    let tracks = raw
        .tracks
        .map(|t| t.items)
        .unwrap_or_default()
        .into_iter()
        .map(|raw_track| {
            let artists: Vec<String> = raw_track.artists.into_iter().map(|a| a.name).collect();
            let artist_display = artists.join(", ");
            let album_art = raw_track
                .album
                .images
                .iter()
                .find(|img| !img.url.is_empty())
                .map(|img| img.url.clone());

            TrackItem {
                id: raw_track.id,
                name: raw_track.name,
                uri: raw_track.uri,
                artists,
                artist_display,
                album_name: raw_track.album.name,
                album_art,
                duration_ms: raw_track.duration_ms,
                explicit: raw_track.explicit,
            }
        })
        .collect();

    Ok(Json(SearchResponse {
        query: query_str.to_string(),
        tracks,
    }))
}

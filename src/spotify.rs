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

    let is_demo_room = if let Some(ref code) = params.room_code {
        let rooms = state.rooms.read().await;
        rooms.get(&code.to_uppercase()).map_or(false, |r| r.is_demo)
    } else {
        false
    };

    if is_demo_room {
        let tracks = search_demo_tracks(query_str);
        return Ok(Json(SearchResponse {
            query: query_str.to_string(),
            tracks,
        }));
    }

    let access_token = match get_access_token_for_search(
        &state,
        &headers,
        params.room_code.as_deref(),
    )
    .await {
        Ok(t) => t,
        Err(_) => {
            tracing::info!("Using demo tracks fallback for search '{}' (no active Spotify token)", query_str);
            return Ok(Json(SearchResponse {
                query: query_str.to_string(),
                tracks: search_demo_tracks(query_str),
            }));
        }
    };

    let limit = params.limit.unwrap_or(10).clamp(1, 10);

    let search_url = format!(
        "https://api.spotify.com/v1/search?q={}&type=track&limit={}",
        url::form_urlencoded::byte_serialize(query_str.as_bytes()).collect::<String>(),
        limit
    );

    let mut spotify_res = match state
        .http_client
        .get(&search_url)
        .bearer_auth(&access_token)
        .send()
        .await {
            Ok(r) => r,
            Err(_) => {
                return Ok(Json(SearchResponse {
                    query: query_str.to_string(),
                    tracks: search_demo_tracks(query_str),
                }));
            }
        };

    // If first attempt failed (e.g. dev mode token restriction or rate limit), retry with client credentials or fallback
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
        tracing::info!("Spotify API search returned error ({}), gracefully returning demo tracks...", spotify_res.status());
        return Ok(Json(SearchResponse {
            query: query_str.to_string(),
            tracks: search_demo_tracks(query_str),
        }));
    }

    let raw: SpotifyRawSearchResponse = match spotify_res.json().await {
        Ok(r) => r,
        Err(_) => {
            return Ok(Json(SearchResponse {
                query: query_str.to_string(),
                tracks: search_demo_tracks(query_str),
            }));
        }
    };

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

pub fn get_demo_tracks() -> Vec<TrackItem> {
    vec![
        TrackItem {
            id: "demo_1".into(),
            name: "Blinding Lights".into(),
            uri: "spotify:track:0VjIjW4GlUZAMYd2vXMi3b".into(),
            artists: vec!["The Weeknd".into()],
            artist_display: "The Weeknd".into(),
            album_name: "After Hours".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b2738863bc11d2aa12b54f5aeb36".into()),
            duration_ms: 200000,
            explicit: false,
        },
        TrackItem {
            id: "demo_2".into(),
            name: "Levitating".into(),
            uri: "spotify:track:463CkQjx2Zk1yXoBuierM9".into(),
            artists: vec!["Dua Lipa".into()],
            artist_display: "Dua Lipa".into(),
            album_name: "Future Nostalgia".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b273bd26ede1ae69327010d49946".into()),
            duration_ms: 203000,
            explicit: false,
        },
        TrackItem {
            id: "demo_3".into(),
            name: "One More Time".into(),
            uri: "spotify:track:0DiWol3AO6WpXZgp0gGlNW".into(),
            artists: vec!["Daft Punk".into()],
            artist_display: "Daft Punk".into(),
            album_name: "Discovery".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b27364658d30e5270c538cb3c0fa".into()),
            duration_ms: 320000,
            explicit: false,
        },
        TrackItem {
            id: "demo_4".into(),
            name: "As It Was".into(),
            uri: "spotify:track:4LRPiXqCikLlN15c3yImP7".into(),
            artists: vec!["Harry Styles".into()],
            artist_display: "Harry Styles".into(),
            album_name: "Harry's House".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b2732e8ed79e177ff6011076f5f0".into()),
            duration_ms: 167000,
            explicit: false,
        },
        TrackItem {
            id: "demo_5".into(),
            name: "Levels".into(),
            uri: "spotify:track:5UqCQaD9rIk42iEvP5Utbv".into(),
            artists: vec!["Avicii".into()],
            artist_display: "Avicii".into(),
            album_name: "Levels".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b2736b7617b0d778d975ab4fb27d".into()),
            duration_ms: 200000,
            explicit: false,
        },
        TrackItem {
            id: "demo_6".into(),
            name: "Don't Stop Me Now".into(),
            uri: "spotify:track:5T8KuEkjh26IDQg7G8q0zZ".into(),
            artists: vec!["Queen".into()],
            artist_display: "Queen".into(),
            album_name: "Jazz".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b2737c3905cf714fb296ad50eb55".into()),
            duration_ms: 209000,
            explicit: false,
        },
        TrackItem {
            id: "demo_7".into(),
            name: "September".into(),
            uri: "spotify:track:2grjqo0Frpf25SaISBkuOJ".into(),
            artists: vec!["Earth, Wind & Fire".into()],
            artist_display: "Earth, Wind & Fire".into(),
            album_name: "The Best of Earth, Wind & Fire".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b273e3bfda3733a8a3a2d3989c44".into()),
            duration_ms: 215000,
            explicit: false,
        },
        TrackItem {
            id: "demo_8".into(),
            name: "Can't Hold Us".into(),
            uri: "spotify:track:3bidbhpOiOYv1Y5HNDRJ7n".into(),
            artists: vec!["Macklemore & Ryan Lewis".into(), "Ray Dalton".into()],
            artist_display: "Macklemore & Ryan Lewis, Ray Dalton".into(),
            album_name: "The Heist".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b27329971bc32204c352ee8a209b".into()),
            duration_ms: 258000,
            explicit: false,
        },
        TrackItem {
            id: "demo_9".into(),
            name: "Gimme! Gimme! Gimme!".into(),
            uri: "spotify:track:3vkQ5DAB1qQMYO4Mr9zJN2".into(),
            artists: vec!["ABBA".into()],
            artist_display: "ABBA".into(),
            album_name: "Voulez-Vous".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b2737c3583ca86e0daeaab579477".into()),
            duration_ms: 288000,
            explicit: false,
        },
        TrackItem {
            id: "demo_10".into(),
            name: "Titanium (feat. Sia)".into(),
            uri: "spotify:track:0TDLuuLlV54CkRRUOahJb4".into(),
            artists: vec!["David Guetta".into(), "Sia".into()],
            artist_display: "David Guetta, Sia".into(),
            album_name: "Nothing but the Beat".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b273981fc47c40d7c71d9d7ec430".into()),
            duration_ms: 245000,
            explicit: false,
        },
        TrackItem {
            id: "demo_11".into(),
            name: "Flowers".into(),
            uri: "spotify:track:0yLdt7AgGvymDIAvpsPLFd".into(),
            artists: vec!["Miley Cyrus".into()],
            artist_display: "Miley Cyrus".into(),
            album_name: "Endless Summer Vacation".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b273f429549123dbe8552764ba1d".into()),
            duration_ms: 200000,
            explicit: false,
        },
        TrackItem {
            id: "demo_12".into(),
            name: "24K Magic".into(),
            uri: "spotify:track:6b8Be6gHG9EILpHNv7AqNe".into(),
            artists: vec!["Bruno Mars".into()],
            artist_display: "Bruno Mars".into(),
            album_name: "24K Magic".into(),
            album_art: Some("https://i.scdn.co/image/ab67616d0000b273ef4d402e3b2e5668b5930e1a".into()),
            duration_ms: 225000,
            explicit: false,
        },
    ]
}

pub fn search_demo_tracks(query: &str) -> Vec<TrackItem> {
    let q = query.trim().to_lowercase();
    let all = get_demo_tracks();
    if q.is_empty() {
        return all;
    }

    let filtered: Vec<TrackItem> = all
        .iter()
        .filter(|t| {
            t.name.to_lowercase().contains(&q)
                || t.artist_display.to_lowercase().contains(&q)
                || t.album_name.to_lowercase().contains(&q)
        })
        .cloned()
        .collect();

    if !filtered.is_empty() {
        filtered
    } else {
        // Fallback: return popular songs if query has no direct match
        all.into_iter().take(6).collect()
    }
}

pub fn get_default_demo_track(idx: usize) -> crate::room::CurrentlyPlayingInfo {
    let demos = get_demo_tracks();
    let track = &demos[idx % demos.len()];
    crate::room::CurrentlyPlayingInfo {
        track_id: track.id.clone(),
        name: track.name.clone(),
        uri: track.uri.clone(),
        artist_display: track.artist_display.clone(),
        album_name: track.album_name.clone(),
        album_art: track.album_art.clone(),
        duration_ms: track.duration_ms,
        progress_ms: 0,
        is_playing: true,
        remaining_ms: track.duration_ms,
    }
}

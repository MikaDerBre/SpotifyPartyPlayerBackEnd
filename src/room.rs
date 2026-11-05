use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{extract_session_token, AppState};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomMember {
    pub id: String,
    pub name: String,
    pub is_host: bool,
    pub avatar_url: Option<String>,
    pub joined_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub track_id: String,
    pub name: String,
    pub uri: String,
    pub artist_display: String,
    pub album_name: String,
    pub album_art: Option<String>,
    pub duration_ms: u64,
    pub added_by_name: String,
    pub added_by_id: String,
    pub added_at: u64,
    pub votes: usize,
    pub voters: HashSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CurrentlyPlayingInfo {
    pub track_id: String,
    pub name: String,
    pub uri: String,
    pub artist_display: String,
    pub album_name: String,
    pub album_art: Option<String>,
    pub duration_ms: u64,
    pub progress_ms: u64,
    pub is_playing: bool,
    pub remaining_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Room {
    pub code: String,
    pub name: String,
    pub host_id: String,
    pub host_name: String,
    pub members: Vec<RoomMember>,
    pub queue: Vec<QueueItem>,
    pub created_at: u64,
    pub currently_playing: Option<CurrentlyPlayingInfo>,
    pub enqueued_for_track_id: Option<String>,
    #[serde(default)]
    pub is_demo: bool,
    #[serde(skip)]
    pub last_polled_at: Option<std::time::Instant>,
    #[serde(skip)]
    pub last_activity_at: Option<std::time::Instant>,
}

impl Room {
    pub fn sort_queue(&mut self) {
        self.queue.sort_by(|a, b| {
            // Sort by votes descending (highest first)
            b.votes
                .cmp(&a.votes)
                // If equal, older added tracks first (FIFO)
                .then_with(|| a.added_at.cmp(&b.added_at))
        });
    }
}

#[derive(Deserialize)]
pub struct CreateRoomRequest {
    pub name: String,
    pub host_name: Option<String>,
    #[serde(default)]
    pub is_demo: bool,
}

#[derive(Deserialize)]
pub struct JoinRoomRequest {
    pub nickname: String,
    pub user_id: Option<String>,
}

#[derive(Deserialize)]
pub struct LeaveRoomRequest {
    pub user_id: String,
}

#[derive(Deserialize)]
pub struct AddToQueueRequest {
    pub track_id: String,
    pub name: String,
    pub uri: String,
    pub artist_display: String,
    pub album_name: String,
    pub album_art: Option<String>,
    pub duration_ms: u64,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
}

#[derive(Deserialize)]
pub struct VoteRequest {
    pub user_id: Option<String>,
}

#[derive(Serialize)]
pub struct RoomResponse {
    pub code: String,
    pub name: String,
    pub host_id: String,
    pub host_name: String,
    pub members: Vec<RoomMember>,
    pub member_count: usize,
    pub queue: Vec<QueueItem>,
    pub created_at: u64,
    pub currently_playing: Option<CurrentlyPlayingInfo>,
    pub is_demo: bool,
}

impl From<&Room> for RoomResponse {
    fn from(r: &Room) -> Self {
        Self {
            code: r.code.clone(),
            name: r.name.clone(),
            host_id: r.host_id.clone(),
            host_name: r.host_name.clone(),
            members: r.members.clone(),
            member_count: r.members.len(),
            queue: r.queue.clone(),
            created_at: r.created_at,
            currently_playing: r.currently_playing.clone(),
            is_demo: r.is_demo,
        }
    }
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn generate_room_code() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..6)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

// POST /api/rooms - Create a new party room
pub async fn create_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateRoomRequest>,
) -> Result<Json<RoomResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Check if the user is authenticated with Spotify (or creating a demo room)
    let (host_id, host_name, avatar_url) = if payload.is_demo {
        let hname = payload
            .host_name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Demo DJ".to_string());
        (
            "demo_host".to_string(),
            hname,
            Some("https://i.scdn.co/image/ab67616d0000b2738863bc11d2aa12b54f5aeb36".to_string()),
        )
    } else if let Some(token) = extract_session_token(&headers) {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&token) {
            let hid = session.user.id.clone();
            let hname = session.user.display_name.clone().unwrap_or_else(|| {
                payload.host_name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Host".to_string())
            });
            let av = session.user.images.first().map(|img| img.url.clone());
            (hid, hname, av)
        } else {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "Deine Spotify-Sitzung ist abgelaufen oder ungültig. Bitte melde dich erneut mit Spotify an."
                })),
            ));
        }
    } else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "Nur mit Spotify angemeldete Nutzer können einen Party-Raum erstellen (oder nutze den Demo-Modus)."
            })),
        ));
    };

    let mut rooms = state.rooms.write().await;

    // Generate unique 6-char room code
    let mut code = generate_room_code();
    while rooms.contains_key(&code) {
        code = generate_room_code();
    }

    let now = current_timestamp();
    let host_member = RoomMember {
        id: host_id.clone(),
        name: host_name.clone(),
        is_host: true,
        avatar_url,
        joined_at: now,
    };

    let initial_cp = if payload.is_demo {
        Some(crate::spotify::get_default_demo_track(0))
    } else {
        None
    };

    let mut room = Room {
        code: code.clone(),
        name: if payload.name.trim().is_empty() {
            if payload.is_demo {
                "Showcase Demo Party".to_string()
            } else {
                format!("{}'s Party", host_name)
            }
        } else {
            payload.name.trim().to_string()
        },
        host_id,
        host_name,
        members: vec![host_member],
        queue: Vec::new(),
        created_at: now,
        currently_playing: initial_cp,
        enqueued_for_track_id: None,
        is_demo: payload.is_demo,
        last_polled_at: None,
        last_activity_at: Some(std::time::Instant::now()),
    };
    room.sort_queue();

    let response = RoomResponse::from(&room);
    rooms.insert(code, room);

    tracing::info!("Created room: {} ({})", response.name, response.code);
    Ok(Json(response))
}

// GET /api/rooms/:code - Get room details
pub async fn get_room(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<RoomResponse>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let mut rooms = state.rooms.write().await;

    match rooms.get_mut(&code_upper) {
        Some(room) => {
            room.last_activity_at = Some(std::time::Instant::now());
            Ok(Json(RoomResponse::from(&*room)))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Raum nicht gefunden. Bitte prüfe den Code." })),
        )),
    }
}

// POST /api/rooms/:code/join - Join an existing room
pub async fn join_room(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<JoinRoomRequest>,
) -> Result<Json<RoomResponse>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let mut rooms = state.rooms.write().await;

    let room = match rooms.get_mut(&code_upper) {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Raum nicht gefunden." })),
            ));
        }
    };

    let mut user_id = payload
        .user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut name = payload.nickname.trim().to_string();
    let mut avatar_url = None;

    if let Some(token) = extract_session_token(&headers) {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&token) {
            user_id = session.user.id.clone();
            if name.is_empty() {
                if let Some(ref display_name) = session.user.display_name {
                    name = display_name.clone();
                }
            }
            if let Some(img) = session.user.images.first() {
                avatar_url = Some(img.url.clone());
            }
        }
    }

    if name.is_empty() {
        name = "Party Gast".to_string();
    }

    // Check if user is already in the room
    let already_member = room.members.iter_mut().find(|m| m.id == user_id);
    if let Some(member) = already_member {
        member.name = name;
        if avatar_url.is_some() {
            member.avatar_url = avatar_url;
        }
    } else {
        let is_host = user_id == room.host_id;
        room.members.push(RoomMember {
            id: user_id,
            name,
            is_host,
            avatar_url,
            joined_at: current_timestamp(),
        });
    }

    Ok(Json(RoomResponse::from(&*room)))
}

// POST /api/rooms/:code/leave - Leave a room
pub async fn leave_room(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(payload): Json<LeaveRoomRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let mut rooms = state.rooms.write().await;

    if let Some(room) = rooms.get_mut(&code_upper) {
        room.members.retain(|m| m.id != payload.user_id);
        if room.members.is_empty() {
            rooms.remove(&code_upper);
        }
    }

    Ok(Json(serde_json::json!({ "success": true })))
}

// DELETE /api/rooms/:code - Delete/close a room
pub async fn delete_room(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let mut rooms = state.rooms.write().await;
    rooms.remove(&code_upper);
    Ok(Json(serde_json::json!({ "success": true })))
}

// POST /api/rooms/:code/queue - Add a track to the room queue
pub async fn add_to_queue(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<AddToQueueRequest>,
) -> Result<Json<RoomResponse>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let mut rooms = state.rooms.write().await;

    let room = match rooms.get_mut(&code_upper) {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Raum nicht gefunden." })),
            ));
        }
    };

    let mut user_id = payload
        .user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| "guest".to_string());
    let mut user_name = payload
        .user_name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Gast".to_string());

    if let Some(token) = extract_session_token(&headers) {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&token) {
            user_id = session.user.id.clone();
            if let Some(ref name) = session.user.display_name {
                user_name = name.clone();
            }
        }
    }

    // Check if the track is already in the queue
    if let Some(existing_item) = room.queue.iter_mut().find(|item| item.track_id == payload.track_id) {
        // Automatically add a vote from this user if not already voted
        existing_item.voters.insert(user_id);
        existing_item.votes = existing_item.voters.len();
    } else {
        let mut voters = HashSet::new();
        voters.insert(user_id.clone());

        let new_item = QueueItem {
            id: uuid::Uuid::new_v4().to_string(),
            track_id: payload.track_id,
            name: payload.name,
            uri: payload.uri,
            artist_display: payload.artist_display,
            album_name: payload.album_name,
            album_art: payload.album_art,
            duration_ms: payload.duration_ms,
            added_by_name: user_name,
            added_by_id: user_id,
            added_at: current_timestamp(),
            votes: voters.len(),
            voters,
        };
        room.queue.push(new_item);
    }

    // Sort queue: highest votes first!
    room.sort_queue();

    tracing::info!("Track added/voted in queue for room {}", code_upper);
    Ok(Json(RoomResponse::from(&*room)))
}

// POST /api/rooms/:code/queue/:track_id/vote - Upvote a track in the queue
pub async fn vote_track(
    State(state): State<AppState>,
    Path((code, track_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<VoteRequest>,
) -> Result<Json<RoomResponse>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let mut rooms = state.rooms.write().await;

    let room = match rooms.get_mut(&code_upper) {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Raum nicht gefunden." })),
            ));
        }
    };

    let mut user_id = payload
        .user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| "guest".to_string());

    if let Some(token) = extract_session_token(&headers) {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&token) {
            user_id = session.user.id.clone();
        }
    }

    let track_found = room.queue.iter_mut().find(|item| item.track_id == track_id);
    match track_found {
        Some(item) => {
            item.voters.insert(user_id);
            item.votes = item.voters.len();
        }
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Song nicht in der Queue gefunden." })),
            ));
        }
    }

    // Re-sort queue with new vote count!
    room.sort_queue();

    Ok(Json(RoomResponse::from(&*room)))
}

// DELETE /api/rooms/:code/queue/:track_id - Remove a track from queue (e.g. Host)
pub async fn remove_from_queue(
    State(state): State<AppState>,
    Path((code, track_id)): Path<(String, String)>,
) -> Result<Json<RoomResponse>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let mut rooms = state.rooms.write().await;

    let room = match rooms.get_mut(&code_upper) {
        Some(r) => r,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Raum nicht gefunden." })),
            ));
        }
    };

    room.queue.retain(|item| item.track_id != track_id);
    room.sort_queue();

    Ok(Json(RoomResponse::from(&*room)))
}

// POST /api/rooms/:code/playback/skip - Skip currently playing track (Host only)
pub async fn skip_current_track(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();

    let (host_id, is_demo) = {
        let rooms = state.rooms.read().await;
        let room = rooms.get(&code_upper).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Raum nicht gefunden" })),
            )
        })?;
        (room.host_id.clone(), room.is_demo)
    };

    // In demo rooms, skip immediately advances the simulated player to the #1 voted queue song!
    if is_demo {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(&code_upper) {
            room.sort_queue();
            let next_cp = if !room.queue.is_empty() {
                let next = room.queue.remove(0);
                CurrentlyPlayingInfo {
                    track_id: next.track_id,
                    name: next.name,
                    uri: next.uri,
                    artist_display: next.artist_display,
                    album_name: next.album_name,
                    album_art: next.album_art,
                    duration_ms: next.duration_ms,
                    progress_ms: 0,
                    is_playing: true,
                    remaining_ms: next.duration_ms,
                }
            } else {
                let all_demos = crate::spotify::get_demo_tracks();
                let cur_id = room.currently_playing.as_ref().map(|c| c.track_id.as_str()).unwrap_or("");
                let next_idx = all_demos
                    .iter()
                    .position(|t| t.id == cur_id)
                    .map(|i| (i + 1) % all_demos.len())
                    .unwrap_or(0);
                crate::spotify::get_default_demo_track(next_idx)
            };
            room.currently_playing = Some(next_cp);
            room.last_polled_at = Some(std::time::Instant::now());
        }
        return Ok(Json(serde_json::json!({ "success": true, "message": "Demo-Song übersprungen" })));
    }

    let token = extract_session_token(&headers).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Nicht authentifiziert" })),
        )
    })?;

    {
        let sessions = state.sessions.read().await;
        let session = sessions.get(&token).ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "Ungültige Sitzung" })),
            )
        })?;
        if session.user.id != host_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "Nur der Host darf Songs überspringen" })),
            ));
        }
    }

    let access_token = crate::auth::ensure_host_token(&state, &host_id).await.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Spotify-Authentifizierung des Hosts abgelaufen" })),
        )
    })?;

    let (song_to_enqueue, current_track_id) = {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(&code_upper) {
            let cur_tid = room.currently_playing.as_ref().map(|cp| cp.track_id.clone());
            let already_enqueued = cur_tid.is_some() && room.enqueued_for_track_id == cur_tid;

            if !already_enqueued {
                room.sort_queue();
                if !room.queue.is_empty() {
                    let next = room.queue.remove(0);
                    (Some(next), cur_tid)
                } else {
                    (None, cur_tid)
                }
            } else {
                (None, cur_tid)
            }
        } else {
            (None, None)
        }
    };

    if let Some(next_song) = song_to_enqueue {
        let queue_url = format!(
            "https://api.spotify.com/v1/me/player/queue?uri={}",
            crate::auth::urlencoding(&next_song.uri)
        );
        let _ = state
            .http_client
            .post(&queue_url)
            .bearer_auth(&access_token)
            .header("Content-Length", "0")
            .send()
            .await;

        if let Some(cur_tid) = current_track_id {
            let mut rooms = state.rooms.write().await;
            if let Some(room) = rooms.get_mut(&code_upper) {
                room.enqueued_for_track_id = Some(cur_tid);
            }
        }
    }

    let skip_res = state
        .http_client
        .post("https://api.spotify.com/v1/me/player/next")
        .bearer_auth(&access_token)
        .header("Content-Length", "0")
        .send()
        .await;

    match skip_res {
        Ok(res) if res.status().is_success() || res.status() == StatusCode::NO_CONTENT => {
            tracing::info!("Host successfully skipped track for room {}", code_upper);
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            crate::player_sync::sync_room(&state, &code_upper).await;
            Ok(Json(serde_json::json!({ "success": true, "message": "Song übersprungen" })))
        }
        Ok(res) => {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            tracing::warn!("Spotify skip returned status {}: {}", status, body);
            Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("Spotify konnte nicht überspringen: {}", body)
                })),
            ))
        }
        Err(e) => {
            tracing::error!("Network error skipping Spotify track: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Netzwerkfehler beim Überspringen" })),
            ))
        }
    }
}

// POST /api/rooms/:code/link-host - Link current authenticated Spotify user as host of this room
pub async fn link_host(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RoomResponse>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();

    let token = extract_session_token(&headers).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Nicht mit Spotify angemeldet" })),
        )
    })?;

    let session_user = {
        let sessions = state.sessions.read().await;
        let session = sessions.get(&token).ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "Ungültige oder abgelaufene Spotify-Sitzung" })),
            )
        })?;
        session.user.clone()
    };

    {
        let mut rooms = state.rooms.write().await;
        let room = rooms.get_mut(&code_upper).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Raum nicht gefunden" })),
            )
        })?;

        tracing::info!(
            "Linking Spotify host '{}' ({}) to room {}",
            session_user.display_name.as_deref().unwrap_or(&session_user.id),
            session_user.id,
            code_upper
        );

        room.host_id = session_user.id.clone();
        if let Some(ref name) = session_user.display_name {
            room.host_name = name.clone();
        }

        // Update or add host in members list
        let mut found_member = false;
        for member in &mut room.members {
            if member.is_host {
                member.id = session_user.id.clone();
                member.name = room.host_name.clone();
                if let Some(img) = session_user.images.first() {
                    member.avatar_url = Some(img.url.clone());
                }
                found_member = true;
                break;
            }
        }
        if !found_member {
            room.members.push(RoomMember {
                id: session_user.id.clone(),
                name: room.host_name.clone(),
                is_host: true,
                avatar_url: session_user.images.first().map(|img| img.url.clone()),
                joined_at: current_timestamp(),
            });
        }
    }

    // Immediately trigger a sync for this room
    crate::player_sync::sync_room(&state, &code_upper).await;

    let rooms = state.rooms.read().await;
    let room = rooms.get(&code_upper).unwrap();
    Ok(Json(RoomResponse::from(room)))
}

// GET /api/rooms/:code/player-diagnostics - Diagnostic check for Spotify playback
pub async fn get_player_diagnostics(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let code_upper = code.to_uppercase();
    let host_id = {
        let rooms = state.rooms.read().await;
        let room = rooms.get(&code_upper).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Raum nicht gefunden" })),
            )
        })?;
        room.host_id.clone()
    };

    let has_session = {
        let sessions = state.sessions.read().await;
        sessions.values().any(|s| s.user.id == host_id)
    };

    let token_opt = crate::auth::ensure_host_token(&state, &host_id).await;
    let has_token = token_opt.is_some();

    let mut spotify_status = None;
    let mut spotify_body = None;
    let mut retry_after_sec = None;

    if let Some(ref access_token) = token_opt {
        if let Ok(res) = state
            .http_client
            .get("https://api.spotify.com/v1/me/player/currently-playing?additional_types=track,episode")
            .bearer_auth(access_token)
            .send()
            .await
        {
            spotify_status = Some(res.status().as_u16());
            retry_after_sec = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok());
            spotify_body = res.text().await.ok();
        }
    }

    let room_cp = {
        let rooms = state.rooms.read().await;
        rooms.get(&code_upper).and_then(|r| r.currently_playing.clone())
    };

    let (rate_limited, wait_secs) = crate::player_sync::is_rate_limited();
    let diagnosis = if rate_limited || spotify_status == Some(429) {
        let wait = wait_secs.max(retry_after_sec.unwrap_or(0)).max(15);
        format!(
            "RATE LIMIT (HTTP 429): Spotify hat vorübergehend zu viele Anfragen erhalten. Cooldown aktiv für noch ca. {} Sekunden. Bitte kurz warten, danach synchronisiert sich die Musik automatisch!",
            wait
        )
    } else if !has_session {
        "FEHLER: Der Host dieses Raums ist aktuell nicht mit Spotify angemeldet (Sitzung fehlt). Bitte melde dich mit Spotify an!".to_string()
    } else if !has_token {
        "FEHLER: Spotify Access Token konnte nicht erneuert werden.".to_string()
    } else if spotify_status == Some(204) {
        "HINWEIS: Spotify antwortet mit 204 No Content. Das bedeutet: Auf dem Spotify-Account des Hosts ist aktuell kein Song aktiv (Player pausiert, geschlossen oder privater Modus aktiv). Bitte starte einen Song in der Spotify-App auf deinem PC oder Handy!".to_string()
    } else if spotify_status == Some(200) {
        "OK: Spotify meldet aktive Wiedergabe.".to_string()
    } else {
        "UNBEKANNT: Prüfe die Spotify-Antwort im Feld 'spotify_response'.".to_string()
    };

    Ok(Json(serde_json::json!({
        "room_code": code_upper,
        "host_id": host_id,
        "host_has_session": has_session,
        "host_has_valid_token": has_token,
        "spotify_http_status": spotify_status,
        "retry_after_seconds": retry_after_sec,
        "spotify_response": spotify_body,
        "room_currently_playing": room_cp,
        "diagnosis": diagnosis
    })))
}



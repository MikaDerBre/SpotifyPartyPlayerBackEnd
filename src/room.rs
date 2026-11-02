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
    let mut host_id = uuid::Uuid::new_v4().to_string();
    let mut host_name = payload
        .host_name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Host".to_string());
    let mut avatar_url = None;

    // Check if the user is authenticated with Spotify
    if let Some(token) = extract_session_token(&headers) {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(&token) {
            host_id = session.user.id.clone();
            if let Some(ref name) = session.user.display_name {
                host_name = name.clone();
            }
            if let Some(img) = session.user.images.first() {
                avatar_url = Some(img.url.clone());
            }
        }
    }

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

    let mut room = Room {
        code: code.clone(),
        name: if payload.name.trim().is_empty() {
            format!("{}'s Party", host_name)
        } else {
            payload.name.trim().to_string()
        },
        host_id,
        host_name,
        members: vec![host_member],
        queue: Vec::new(),
        created_at: now,
        currently_playing: None,
        enqueued_for_track_id: None,
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
    let rooms = state.rooms.read().await;

    match rooms.get(&code_upper) {
        Some(room) => Ok(Json(RoomResponse::from(room))),
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


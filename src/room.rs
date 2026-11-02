use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rand::RngExt;
use serde::{Deserialize, Serialize};
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
pub struct Room {
    pub code: String,
    pub name: String,
    pub host_id: String,
    pub host_name: String,
    pub members: Vec<RoomMember>,
    pub created_at: u64,
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

#[derive(Serialize)]
pub struct RoomResponse {
    pub code: String,
    pub name: String,
    pub host_id: String,
    pub host_name: String,
    pub members: Vec<RoomMember>,
    pub member_count: usize,
    pub created_at: u64,
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
            created_at: r.created_at,
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

    let room = Room {
        code: code.clone(),
        name: if payload.name.trim().is_empty() {
            format!("{}'s Party", host_name)
        } else {
            payload.name.trim().to_string()
        },
        host_id,
        host_name,
        members: vec![host_member],
        created_at: now,
    };

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
        // If room is empty, remove it
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

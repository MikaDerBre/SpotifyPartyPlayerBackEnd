use serde::Deserialize;
use tokio::time::{sleep, Duration};

use crate::auth::{ensure_host_token, urlencoding, AppState};
use crate::room::{CurrentlyPlayingInfo, QueueItem};

#[derive(Deserialize, Debug)]
struct SpotifyCurrentlyPlayingResponse {
    #[serde(default)]
    is_playing: bool,
    #[serde(default)]
    progress_ms: Option<u64>,
    item: Option<SpotifyCurrentlyPlayingItem>,
}

#[derive(Deserialize, Debug)]
struct SpotifyCurrentlyPlayingItem {
    id: Option<String>,
    name: String,
    uri: Option<String>,
    duration_ms: u64,
    #[serde(default)]
    artists: Vec<SpotifyArtistItem>,
    album: Option<SpotifyAlbumItem>,
}

#[derive(Deserialize, Debug)]
struct SpotifyArtistItem {
    name: String,
}

#[derive(Deserialize, Debug)]
struct SpotifyAlbumItem {
    name: Option<String>,
    #[serde(default)]
    images: Vec<SpotifyImageItem>,
}

#[derive(Deserialize, Debug)]
struct SpotifyImageItem {
    url: String,
}

pub fn start_sync_loop(state: AppState) {
    tokio::spawn(async move {
        tracing::info!("Starting background Spotify player sync loop (interval: 2s)...");
        loop {
            sleep(Duration::from_millis(2000)).await;
            sync_active_rooms(&state).await;
        }
    });
}

async fn sync_active_rooms(state: &AppState) {
    let room_codes: Vec<String> = {
        let rooms = state.rooms.read().await;
        rooms.keys().cloned().collect()
    };

    for code in room_codes {
        sync_room(state, &code).await;
    }
}

async fn sync_room(state: &AppState, code: &str) {
    let host_id = {
        let rooms = state.rooms.read().await;
        match rooms.get(code) {
            Some(r) => r.host_id.clone(),
            None => return,
        }
    };

    // Ensure valid host access token
    let access_token = match ensure_host_token(state, &host_id).await {
        Some(token) => token,
        None => return,
    };

    // Fetch currently playing track from Spotify
    let res = match state
        .http_client
        .get("https://api.spotify.com/v1/me/player/currently-playing")
        .bearer_auth(&access_token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("Error polling currently-playing for room {}: {}", code, e);
            return;
        }
    };

    let status = res.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(code) {
            room.currently_playing = None;
        }
        return;
    }

    if !status.is_success() {
        tracing::debug!(
            "Spotify currently-playing returned status {} for room {}",
            status,
            code
        );
        return;
    }

    let cp_data: SpotifyCurrentlyPlayingResponse = match res.json().await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(
                "Failed to parse currently-playing JSON for room {}: {}",
                code,
                e
            );
            return;
        }
    };

    let item = match cp_data.item {
        Some(i) => i,
        None => {
            let mut rooms = state.rooms.write().await;
            if let Some(room) = rooms.get_mut(code) {
                room.currently_playing = None;
            }
            return;
        }
    };

    let track_id = match item.id {
        Some(id) if !id.trim().is_empty() => id,
        _ => return,
    };

    let uri = item
        .uri
        .unwrap_or_else(|| format!("spotify:track:{}", track_id));
    let artist_display = item
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let album_name = item
        .album
        .as_ref()
        .and_then(|a| a.name.clone())
        .unwrap_or_default();
    let album_art = item
        .album
        .as_ref()
        .and_then(|a| a.images.first().map(|img| img.url.clone()));
    let duration_ms = item.duration_ms;
    let progress_ms = cp_data.progress_ms.unwrap_or(0);
    let remaining_ms = duration_ms.saturating_sub(progress_ms);
    let is_playing = cp_data.is_playing;

    let currently_playing = CurrentlyPlayingInfo {
        track_id: track_id.clone(),
        name: item.name.clone(),
        uri: uri.clone(),
        artist_display,
        album_name,
        album_art,
        duration_ms,
        progress_ms,
        is_playing,
        remaining_ms,
    };

    // Auto-enqueue rule: Host playback is active, remaining time <= 20 seconds (20_000 ms),
    // and we haven't already enqueued a song for this current track
    let is_near_end = is_playing && remaining_ms <= 20_000;
    let mut song_to_enqueue: Option<(QueueItem, String)> = None;

    {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(code) {
            room.currently_playing = Some(currently_playing);

            if is_near_end {
                let already_enqueued = room.enqueued_for_track_id.as_deref() == Some(&track_id);
                if !already_enqueued {
                    room.sort_queue();
                    if !room.queue.is_empty() {
                        let next = room.queue.remove(0);
                        tracing::info!(
                            "Near end trigger for room {}: current track '{}' has {}s remaining. Preparing to enqueue '#1' song '{}' ({} votes)...",
                            code,
                            item.name,
                            remaining_ms / 1000,
                            next.name,
                            next.votes
                        );
                        song_to_enqueue = Some((next, track_id.clone()));
                    }
                }
            }
        }
    }

    if let Some((next_song, current_tid)) = song_to_enqueue {
        let queue_url = format!(
            "https://api.spotify.com/v1/me/player/queue?uri={}",
            urlencoding(&next_song.uri)
        );

        let queue_res = state
            .http_client
            .post(&queue_url)
            .bearer_auth(&access_token)
            .header("Content-Length", "0")
            .send()
            .await;

        match queue_res {
            Ok(r) if r.status().is_success() => {
                tracing::info!(
                    "Seamless transition SUCCESS: Auto-enqueued '#1' song '{}' into Spotify queue for room {} ({}s remaining in '{}')",
                    next_song.name,
                    code,
                    remaining_ms / 1000,
                    item.name
                );
                let mut rooms = state.rooms.write().await;
                if let Some(room) = rooms.get_mut(code) {
                    room.enqueued_for_track_id = Some(current_tid);
                }
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                tracing::warn!(
                    "Failed to auto-enqueue track '{}' into Spotify queue (HTTP {}): {}",
                    next_song.name,
                    status,
                    body
                );
                // Put track back at position 0 so it is not lost
                let mut rooms = state.rooms.write().await;
                if let Some(room) = rooms.get_mut(code) {
                    room.queue.insert(0, next_song);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Network error auto-enqueuing track '{}' into Spotify queue: {}",
                    next_song.name,
                    e
                );
                // Put track back at position 0 so it is not lost
                let mut rooms = state.rooms.write().await;
                if let Some(room) = rooms.get_mut(code) {
                    room.queue.insert(0, next_song);
                }
            }
        }
    }
}

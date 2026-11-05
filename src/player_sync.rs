use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{sleep, Duration};

use crate::auth::{ensure_host_token, urlencoding, AppState};
use crate::room::{CurrentlyPlayingInfo, QueueItem};

static RATE_LIMITED_UNTIL: AtomicU64 = AtomicU64::new(0);

fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn is_rate_limited() -> (bool, u64) {
    let until = RATE_LIMITED_UNTIL.load(Ordering::Relaxed);
    let now = current_time_secs();
    if now < until {
        (true, until - now)
    } else {
        (false, 0)
    }
}

pub fn set_rate_limit(seconds: u64) {
    let until = current_time_secs() + seconds;
    RATE_LIMITED_UNTIL.store(until, Ordering::Relaxed);
}

#[derive(Deserialize, Debug)]
struct SpotifyCurrentlyPlayingResponse {
    #[serde(default)]
    is_playing: bool,
    #[serde(default)]
    progress_ms: Option<u64>,
    #[serde(default)]
    item: Option<SpotifyCurrentlyPlayingItem>,
    #[serde(default)]
    #[allow(dead_code)]
    currently_playing_type: Option<String>,
}

#[derive(Deserialize, Debug)]
struct SpotifyCurrentlyPlayingItem {
    id: Option<String>,
    #[serde(default)]
    name: String,
    uri: Option<String>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    artists: Vec<SpotifyArtistItem>,
    album: Option<SpotifyAlbumItem>,
}

#[derive(Deserialize, Debug)]
struct SpotifyArtistItem {
    #[serde(default)]
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
    #[serde(default)]
    url: String,
}

pub fn start_sync_loop(state: AppState) {
    tokio::spawn(async move {
        tracing::info!("Starting background Spotify player sync loop (interval: 3.5s)...");
        loop {
            sleep(Duration::from_millis(3500)).await;
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

async fn sync_demo_room(state: &AppState, code: &str) {
    let mut rooms = state.rooms.write().await;
    let room = match rooms.get_mut(code) {
        Some(r) if r.is_demo => r,
        _ => return,
    };

    let is_viewed = room.last_activity_at.map_or(true, |act| act.elapsed() < Duration::from_secs(600));
    if !is_viewed {
        return;
    }

    let now_instant = std::time::Instant::now();
    let elapsed_ms = match room.last_polled_at {
        Some(last) => last.elapsed().as_millis() as u64,
        None => 3500,
    };
    room.last_polled_at = Some(now_instant);

    if room.currently_playing.is_none() {
        room.currently_playing = Some(crate::spotify::get_default_demo_track(0));
        return;
    }

    let should_advance_track = if let Some(ref mut cp) = room.currently_playing {
        if cp.is_playing {
            cp.progress_ms = cp.progress_ms.saturating_add(elapsed_ms);
            if cp.progress_ms >= cp.duration_ms {
                true
            } else {
                cp.remaining_ms = cp.duration_ms.saturating_sub(cp.progress_ms);
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if should_advance_track {
        let cur_id = room.currently_playing.as_ref().map(|cp| cp.track_id.clone()).unwrap_or_default();
        room.sort_queue();
        let next_track = if !room.queue.is_empty() {
            let next = room.queue.remove(0);
            tracing::info!("[DemoSync] Room {}: Transitioning to top voted queue song '{}'!", code, next.name);
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
            let next_idx = all_demos
                .iter()
                .position(|t| t.id == cur_id)
                .map(|i| (i + 1) % all_demos.len())
                .unwrap_or(0);
            crate::spotify::get_default_demo_track(next_idx)
        };
        room.currently_playing = Some(next_track);
    }
}

pub async fn sync_room(state: &AppState, code: &str) {
    let is_demo = {
        let rooms = state.rooms.read().await;
        rooms.get(code).map(|r| r.is_demo).unwrap_or(false)
    };

    if is_demo {
        sync_demo_room(state, code).await;
        return;
    }

    let (rate_limited, wait_secs) = is_rate_limited();
    if rate_limited {
        tracing::debug!(
            "[PlayerSync] Room {}: Skipping Spotify check due to rate limit cooldown ({}s remaining)",
            code,
            wait_secs
        );
        return;
    }

    let (host_id, should_poll) = {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(code) {
            let is_viewed = room.last_activity_at.map_or(true, |act| act.elapsed() < Duration::from_secs(60));
            if !is_viewed {
                return; // Room is inactive, do not waste Spotify API quota
            }

            let min_interval = match &room.currently_playing {
                Some(cp) if cp.is_playing && cp.remaining_ms <= 20_000 => Duration::from_millis(2500),
                Some(cp) if cp.is_playing => Duration::from_secs(8),
                _ => Duration::from_secs(8),
            };

            let can_poll = room.last_polled_at.map_or(true, |t| t.elapsed() >= min_interval);
            if can_poll {
                room.last_polled_at = Some(std::time::Instant::now());
            }
            (room.host_id.clone(), can_poll)
        } else {
            return;
        }
    };

    if !should_poll {
        return;
    }

    // Ensure valid host access token
    let access_token = match ensure_host_token(state, &host_id).await {
        Some(token) => token,
        None => {
            tracing::warn!(
                "[PlayerSync] Room {}: Host '{}' has no active Spotify session! Make sure the host is logged in with Spotify.",
                code,
                host_id
            );
            return;
        }
    };

    // Fetch currently playing track from Spotify (support both tracks and episodes)
    let res = match state
        .http_client
        .get("https://api.spotify.com/v1/me/player/currently-playing?additional_types=track,episode")
        .bearer_auth(&access_token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[PlayerSync] Network error polling currently-playing for room {}: {}", code, e);
            return;
        }
    };

    let status = res.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = res
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);
        let wait = retry_after.max(15);
        set_rate_limit(wait);
        tracing::warn!(
            "[PlayerSync] Spotify HTTP 429 Too Many Requests for room {}. Backing off for {} seconds!",
            code,
            wait
        );
        return;
    }

    if status == reqwest::StatusCode::NO_CONTENT {
        let mut rooms = state.rooms.write().await;
        if let Some(room) = rooms.get_mut(code) {
            room.currently_playing = None;
        }
        return;
    }

    if !status.is_success() {
        let err_body = res.text().await.unwrap_or_default();
        tracing::warn!(
            "[PlayerSync] Spotify currently-playing returned HTTP {} for room {}: {}",
            status,
            code,
            err_body
        );
        return;
    }

    let raw_body = match res.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("[PlayerSync] Failed to read body for room {}: {}", code, e);
            return;
        }
    };

    let cp_data: SpotifyCurrentlyPlayingResponse = match serde_json::from_str(&raw_body) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                "[PlayerSync] Failed to parse currently-playing JSON for room {}: {} - Raw: {}",
                code,
                e,
                raw_body
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
        _ => item.uri.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
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

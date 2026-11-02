use std::env;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub spotify_client_id: String,
    pub spotify_client_secret: String,
    pub spotify_redirect_uri: String,
    pub frontend_url: String,
    pub port: u16,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let spotify_client_id = env::var("SPOTIFY_CLIENT_ID").unwrap_or_default();
        let spotify_client_secret = env::var("SPOTIFY_CLIENT_SECRET").unwrap_or_default();
        let spotify_redirect_uri = env::var("SPOTIFY_REDIRECT_URI")
            .unwrap_or_else(|_| "http://127.0.0.1:3000/api/auth/spotify/callback".to_string());
        let frontend_url = env::var("FRONTEND_URL")
            .unwrap_or_else(|_| "http://localhost:9000".to_string());
        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3000);

        Self {
            spotify_client_id,
            spotify_client_secret,
            spotify_redirect_uri,
            frontend_url,
            port,
        }
    }

    pub fn is_spotify_configured(&self) -> bool {
        !self.spotify_client_id.is_empty() 
            && !self.spotify_client_secret.is_empty()
            && self.spotify_client_id != "your_spotify_client_id_here"
    }
}

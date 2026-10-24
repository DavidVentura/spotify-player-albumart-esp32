use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::Deserialize;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
// `market=from_token` drops the per-track `available_markets` lists, which are
// most of the response size
const NOW_PLAYING_URL: &str = "https://api.spotify.com/v1/me/player/currently-playing?market=from_token&additional_types=track,episode";
const IMAGE_URL_PREFIX: &str = "https://i.scdn.co/";
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCredentials {
    pub id: String,
    pub secret: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshToken(String);

impl RefreshToken {
    pub fn new(token: String) -> Self {
        RefreshToken(token)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AccessToken(String);

#[derive(Debug, PartialEq, Eq)]
struct TokenGrant {
    access_token: AccessToken,
    expires_in: Duration,
    /// Spotify may rotate the refresh token on any refresh.
    rotated_refresh_token: Option<RefreshToken>,
}

/// A cover on Spotify's image CDN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageUrl(String);

impl ImageUrl {
    fn parse(url: &str) -> Result<Self, SpotifyError> {
        if !url.starts_with(IMAGE_URL_PREFIX) {
            return Err(SpotifyError::UnexpectedImageUrl(url.to_string()));
        }
        Ok(ImageUrl(url.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Artwork {
    Nothing,
    Image(ImageUrl),
}

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only read through Debug when logged")]
pub enum SpotifyError {
    Request(reqwest::Error),
    Unauthorized,
    Status(StatusCode),
    Json(serde_json::Error),
    UnexpectedImageUrl(String),
}

impl From<reqwest::Error> for SpotifyError {
    fn from(e: reqwest::Error) -> Self {
        SpotifyError::Request(e)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct CurrentlyPlaying {
    is_playing: bool,
    item: Option<Item>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Item {
    Track { album: Album },
    Episode { images: Vec<Image> },
}

#[derive(Deserialize)]
struct Album {
    images: Vec<Image>,
}

#[derive(Deserialize)]
struct Image {
    url: String,
    width: Option<u32>,
}

struct Access {
    token: AccessToken,
    expires_at: Instant,
}

pub struct SpotifyClient {
    http: reqwest::Client,
    credentials: ClientCredentials,
    refresh_token: RefreshToken,
    access: Option<Access>,
}

impl SpotifyClient {
    pub fn new(credentials: ClientCredentials, refresh_token: RefreshToken) -> Self {
        SpotifyClient {
            http: reqwest::Client::new(),
            credentials,
            refresh_token,
            access: None,
        }
    }

    pub async fn now_playing(&mut self) -> Result<Artwork, SpotifyError> {
        let token = self.access_token().await?;
        let response = self
            .http
            .get(NOW_PLAYING_URL)
            .bearer_auth(&token.0)
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        let artwork = parse_now_playing(status, &body);
        if let Err(SpotifyError::Unauthorized) = artwork {
            self.access = None;
        }
        artwork
    }

    pub async fn image(&self, url: &ImageUrl) -> Result<Vec<u8>, SpotifyError> {
        let response = self.http.get(&url.0).send().await?;
        if response.status() != StatusCode::OK {
            return Err(SpotifyError::Status(response.status()));
        }
        Ok(response.bytes().await?.to_vec())
    }

    async fn access_token(&mut self) -> Result<AccessToken, SpotifyError> {
        if let Some(access) = &self.access
            && Instant::now() < access.expires_at
        {
            return Ok(access.token.clone());
        }
        let response = self
            .http
            .post(TOKEN_URL)
            .basic_auth(&self.credentials.id, Some(&self.credentials.secret))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.0.as_str()),
            ])
            .send()
            .await?;
        let status = response.status();
        let grant = parse_token_response(status, &response.bytes().await?)?;
        if let Some(rotated) = grant.rotated_refresh_token {
            self.refresh_token = rotated;
        }
        self.access = Some(Access {
            token: grant.access_token.clone(),
            expires_at: Instant::now() + grant.expires_in - TOKEN_EXPIRY_MARGIN,
        });
        Ok(grant.access_token)
    }
}

fn parse_token_response(status: StatusCode, body: &[u8]) -> Result<TokenGrant, SpotifyError> {
    if status != StatusCode::OK {
        return Err(SpotifyError::Status(status));
    }
    let token: TokenResponse = serde_json::from_slice(body).map_err(SpotifyError::Json)?;
    Ok(TokenGrant {
        access_token: AccessToken(token.access_token),
        expires_in: Duration::from_secs(token.expires_in),
        rotated_refresh_token: token.refresh_token.map(RefreshToken),
    })
}

fn parse_now_playing(status: StatusCode, body: &[u8]) -> Result<Artwork, SpotifyError> {
    match status {
        StatusCode::NO_CONTENT => return Ok(Artwork::Nothing),
        StatusCode::UNAUTHORIZED => return Err(SpotifyError::Unauthorized),
        StatusCode::OK => {}
        status => return Err(SpotifyError::Status(status)),
    }
    let playing: CurrentlyPlaying = serde_json::from_slice(body).map_err(SpotifyError::Json)?;
    if !playing.is_playing {
        return Ok(Artwork::Nothing);
    }
    let images = match playing.item {
        None => return Ok(Artwork::Nothing),
        Some(Item::Track { album }) => album.images,
        Some(Item::Episode { images }) => images,
    };
    // The largest cover downsamples into the cleanest 64x64; local files have
    // no images at all
    let Some(largest) = images.iter().max_by_key(|image| image.width) else {
        return Ok(Artwork::Nothing);
    };
    ImageUrl::parse(&largest.url).map(Artwork::Image)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(is_playing: bool, images: &str) -> String {
        format!(
            r#"{{
                "timestamp": 1729700000000,
                "context": {{"type": "playlist", "uri": "spotify:playlist:x"}},
                "progress_ms": 1234,
                "item": {{
                    "album": {{
                        "album_type": "album",
                        "artists": [{{"name": "Some \"Quoted\" Artist", "type": "artist"}}],
                        "images": {images},
                        "name": "Album",
                        "type": "album"
                    }},
                    "artists": [{{"name": "Artist", "type": "artist"}}],
                    "duration_ms": 200000,
                    "name": "Café",
                    "type": "track",
                    "is_local": false
                }},
                "currently_playing_type": "track",
                "actions": {{"disallows": {{"resuming": true}}}},
                "is_playing": {is_playing}
            }}"#
        )
    }

    const ALBUM_IMAGES: &str = r#"[
        {"height": 300, "url": "https://i.scdn.co/image/mid", "width": 300},
        {"height": 640, "url": "https://i.scdn.co/image/big", "width": 640},
        {"height": 64, "url": "https://i.scdn.co/image/small", "width": 64}
    ]"#;

    fn image(url: &str) -> Artwork {
        Artwork::Image(ImageUrl(url.to_string()))
    }

    fn now_playing(body: &str) -> Result<Artwork, SpotifyError> {
        parse_now_playing(StatusCode::OK, body.as_bytes())
    }

    #[test]
    fn parses_token_without_rotation() {
        let body = br#"{
            "access_token": "abc", "token_type": "Bearer",
            "scope": "user-read-currently-playing", "expires_in": 3600
        }"#;
        assert_eq!(
            parse_token_response(StatusCode::OK, body).unwrap(),
            TokenGrant {
                access_token: AccessToken("abc".to_string()),
                expires_in: Duration::from_secs(3600),
                rotated_refresh_token: None,
            }
        );
    }

    #[test]
    fn parses_token_with_rotation() {
        let body = br#"{"access_token": "abc", "expires_in": 3600, "refresh_token": "new"}"#;
        assert_eq!(
            parse_token_response(StatusCode::OK, body)
                .unwrap()
                .rotated_refresh_token,
            Some(RefreshToken::new("new".to_string()))
        );
    }

    #[test]
    fn token_error_status() {
        assert!(matches!(
            parse_token_response(StatusCode::BAD_REQUEST, br#"{"error": "invalid_grant"}"#),
            Err(SpotifyError::Status(StatusCode::BAD_REQUEST))
        ));
    }

    #[test]
    fn playing_track_selects_largest_image() {
        let artwork = now_playing(&track(true, ALBUM_IMAGES)).unwrap();
        assert_eq!(artwork, image("https://i.scdn.co/image/big"));
    }

    #[test]
    fn paused_track_shows_nothing() {
        assert_eq!(
            now_playing(&track(false, ALBUM_IMAGES)).unwrap(),
            Artwork::Nothing
        );
    }

    #[test]
    fn local_file_without_images_shows_nothing() {
        assert_eq!(now_playing(&track(true, "[]")).unwrap(), Artwork::Nothing);
    }

    #[test]
    fn image_on_other_host_is_rejected() {
        let images = r#"[{"height": 64, "url": "https://evil.example/image/x", "width": 64}]"#;
        assert!(matches!(
            now_playing(&track(true, images)),
            Err(SpotifyError::UnexpectedImageUrl(_))
        ));
    }

    #[test]
    fn playing_episode_uses_episode_images() {
        let body = r#"{
            "is_playing": true,
            "currently_playing_type": "episode",
            "item": {
                "type": "episode",
                "name": "Episode",
                "images": [{"height": 64, "url": "https://i.scdn.co/image/ep", "width": 64}],
                "show": {"name": "Show", "images": []}
            }
        }"#;
        assert_eq!(
            now_playing(body).unwrap(),
            image("https://i.scdn.co/image/ep")
        );
    }

    #[test]
    fn ad_without_item_shows_nothing() {
        let body = r#"{"is_playing": true, "currently_playing_type": "ad", "item": null}"#;
        assert_eq!(now_playing(body).unwrap(), Artwork::Nothing);
    }

    #[test]
    fn no_content_shows_nothing() {
        assert_eq!(
            parse_now_playing(StatusCode::NO_CONTENT, b"").unwrap(),
            Artwork::Nothing
        );
    }

    #[test]
    fn expired_token_is_unauthorized() {
        let body = br#"{"error": {"status": 401, "message": "The access token expired"}}"#;
        assert!(matches!(
            parse_now_playing(StatusCode::UNAUTHORIZED, body),
            Err(SpotifyError::Unauthorized)
        ));
    }
}

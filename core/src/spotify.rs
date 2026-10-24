use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::time::Duration;

use base64::Engine;
use serde::Deserialize;

use crate::http::{Authorization, Body, Host, Method, Request, Response, Status};
use crate::thumbnail::SIZE;

const IMAGE_URL_PREFIX: &str = "https://i.scdn.co/";

/// The `Authorization: Basic` credentials of a Spotify app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAuth(String);

impl ClientAuth {
    pub fn new(client_id: &str, client_secret: &str) -> Self {
        let pair = format!("{client_id}:{client_secret}");
        ClientAuth(base64::engine::general_purpose::STANDARD.encode(pair))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshToken(String);

impl RefreshToken {
    pub fn new(token: String) -> Self {
        RefreshToken(token)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessToken(String);

#[derive(Debug, PartialEq, Eq)]
pub struct TokenGrant {
    pub access_token: AccessToken,
    pub expires_in: Duration,
    /// Spotify may rotate the refresh token on any refresh.
    pub rotated_refresh_token: Option<RefreshToken>,
}

/// A 64x64 album or episode cover on Spotify's image CDN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUrl {
    path: String,
}

impl ImageUrl {
    fn parse(url: &str) -> Result<Self, SpotifyError> {
        let Some(rest) = url.strip_prefix(IMAGE_URL_PREFIX) else {
            return Err(SpotifyError::UnexpectedImageUrl(url.to_string()));
        };
        Ok(ImageUrl {
            path: format!("/{rest}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Artwork {
    Nothing,
    Image(ImageUrl),
}

#[derive(Debug)]
pub enum SpotifyError {
    Unauthorized,
    Status(Status),
    Json(serde_json::Error),
    NoThumbnail,
    UnexpectedImageUrl(String),
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
    height: Option<u32>,
}

pub fn refresh_request(client: &ClientAuth, refresh_token: &RefreshToken) -> Request {
    let form = format!(
        "grant_type=refresh_token&refresh_token={}",
        form_urlencode(&refresh_token.0)
    );
    Request {
        method: Method::Post,
        host: Host::Accounts,
        path: "/api/token".to_string(),
        authorization: Authorization::Basic(client.0.clone()),
        body: Body::Form(form),
    }
}

pub fn parse_token_response(response: &Response) -> Result<TokenGrant, SpotifyError> {
    if response.status != Status::OK {
        return Err(SpotifyError::Status(response.status));
    }
    let token: TokenResponse =
        serde_json::from_slice(&response.body).map_err(SpotifyError::Json)?;
    Ok(TokenGrant {
        access_token: AccessToken(token.access_token),
        expires_in: Duration::from_secs(token.expires_in),
        rotated_refresh_token: token.refresh_token.map(RefreshToken),
    })
}

pub fn now_playing_request(token: &AccessToken) -> Request {
    Request {
        method: Method::Get,
        host: Host::Api,
        // `market=from_token` drops the per-track `available_markets` lists,
        // which are most of the response size
        path: "/v1/me/player/currently-playing?market=from_token&additional_types=track,episode"
            .to_string(),
        authorization: Authorization::Bearer(token.0.clone()),
        body: Body::Empty,
    }
}

pub fn parse_now_playing(response: &Response) -> Result<Artwork, SpotifyError> {
    match response.status {
        Status::NO_CONTENT => return Ok(Artwork::Nothing),
        Status::UNAUTHORIZED => return Err(SpotifyError::Unauthorized),
        Status::OK => {}
        status => return Err(SpotifyError::Status(status)),
    }
    let playing: CurrentlyPlaying =
        serde_json::from_slice(&response.body).map_err(SpotifyError::Json)?;
    if !playing.is_playing {
        return Ok(Artwork::Nothing);
    }
    let images = match playing.item {
        None => return Ok(Artwork::Nothing),
        Some(Item::Track { album }) => album.images,
        Some(Item::Episode { images }) => images,
    };
    // Local files have no artwork at all
    if images.is_empty() {
        return Ok(Artwork::Nothing);
    }
    let thumbnail = images
        .iter()
        .find(|image| image.width == Some(SIZE as u32) && image.height == Some(SIZE as u32))
        .ok_or(SpotifyError::NoThumbnail)?;
    ImageUrl::parse(&thumbnail.url).map(Artwork::Image)
}

pub fn image_request(image: &ImageUrl) -> Request {
    Request {
        method: Method::Get,
        host: Host::ImageCdn,
        path: image.path.clone(),
        authorization: Authorization::Anonymous,
        body: Body::Empty,
    }
}

pub fn parse_image_response(response: Response) -> Result<Vec<u8>, SpotifyError> {
    if response.status != Status::OK {
        return Err(SpotifyError::Status(response.status));
    }
    Ok(response.body)
}

fn form_urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(body: &str) -> Response {
        Response {
            status: Status::OK,
            body: body.as_bytes().to_vec(),
        }
    }

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
        {"height": 640, "url": "https://i.scdn.co/image/big", "width": 640},
        {"height": 300, "url": "https://i.scdn.co/image/mid", "width": 300},
        {"height": 64, "url": "https://i.scdn.co/image/small", "width": 64}
    ]"#;

    fn image(path: &str) -> Artwork {
        Artwork::Image(ImageUrl {
            path: path.to_string(),
        })
    }

    #[test]
    fn client_auth_is_base64_of_id_and_secret() {
        assert_eq!(
            ClientAuth::new("client", "secret"),
            ClientAuth("Y2xpZW50OnNlY3JldA==".to_string())
        );
    }

    #[test]
    fn refresh_request_urlencodes_token() {
        let request = refresh_request(
            &ClientAuth::new("client", "secret"),
            &RefreshToken::new("AQ-b_c+d/e=".to_string()),
        );
        assert_eq!(request.host, Host::Accounts);
        assert_eq!(request.method, Method::Post);
        assert_eq!(
            request.body,
            Body::Form("grant_type=refresh_token&refresh_token=AQ-b_c%2Bd%2Fe%3D".to_string())
        );
    }

    #[test]
    fn parses_token_without_rotation() {
        let grant = parse_token_response(&ok(r#"{
            "access_token": "abc", "token_type": "Bearer",
            "scope": "user-read-currently-playing", "expires_in": 3600
        }"#))
        .unwrap();
        assert_eq!(
            grant,
            TokenGrant {
                access_token: AccessToken("abc".to_string()),
                expires_in: Duration::from_secs(3600),
                rotated_refresh_token: None,
            }
        );
    }

    #[test]
    fn parses_token_with_rotation() {
        let grant = parse_token_response(&ok(
            r#"{"access_token": "abc", "expires_in": 3600, "refresh_token": "new"}"#,
        ))
        .unwrap();
        assert_eq!(
            grant.rotated_refresh_token,
            Some(RefreshToken::new("new".to_string()))
        );
    }

    #[test]
    fn token_error_status() {
        let response = Response {
            status: Status(400),
            body: br#"{"error": "invalid_grant"}"#.to_vec(),
        };
        assert!(matches!(
            parse_token_response(&response),
            Err(SpotifyError::Status(Status(400)))
        ));
    }

    #[test]
    fn playing_track_selects_64px_image() {
        let artwork = parse_now_playing(&ok(&track(true, ALBUM_IMAGES))).unwrap();
        assert_eq!(artwork, image("/image/small"));
        assert_eq!(
            image_request(match &artwork {
                Artwork::Image(url) => url,
                Artwork::Nothing => unreachable!(),
            })
            .path,
            "/image/small"
        );
    }

    #[test]
    fn paused_track_shows_nothing() {
        let artwork = parse_now_playing(&ok(&track(false, ALBUM_IMAGES))).unwrap();
        assert_eq!(artwork, Artwork::Nothing);
    }

    #[test]
    fn local_file_without_images_shows_nothing() {
        let artwork = parse_now_playing(&ok(&track(true, "[]"))).unwrap();
        assert_eq!(artwork, Artwork::Nothing);
    }

    #[test]
    fn missing_thumbnail_size_is_an_error() {
        let images = r#"[{"height": 640, "url": "https://i.scdn.co/image/big", "width": 640}]"#;
        assert!(matches!(
            parse_now_playing(&ok(&track(true, images))),
            Err(SpotifyError::NoThumbnail)
        ));
    }

    #[test]
    fn image_on_other_host_is_rejected() {
        let images = r#"[{"height": 64, "url": "https://evil.example/image/x", "width": 64}]"#;
        assert!(matches!(
            parse_now_playing(&ok(&track(true, images))),
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
        assert_eq!(parse_now_playing(&ok(body)).unwrap(), image("/image/ep"));
    }

    #[test]
    fn ad_without_item_shows_nothing() {
        let body = r#"{"is_playing": true, "currently_playing_type": "ad", "item": null}"#;
        assert_eq!(parse_now_playing(&ok(body)).unwrap(), Artwork::Nothing);
    }

    #[test]
    fn no_content_shows_nothing() {
        let response = Response {
            status: Status::NO_CONTENT,
            body: Vec::new(),
        };
        assert_eq!(parse_now_playing(&response).unwrap(), Artwork::Nothing);
    }

    #[test]
    fn expired_token_is_unauthorized() {
        let response = Response {
            status: Status::UNAUTHORIZED,
            body: br#"{"error": {"status": 401, "message": "The access token expired"}}"#.to_vec(),
        };
        assert!(matches!(
            parse_now_playing(&response),
            Err(SpotifyError::Unauthorized)
        ));
    }
}

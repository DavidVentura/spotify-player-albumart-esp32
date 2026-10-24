mod config;
mod cover;
mod device;
mod spotify;
mod web;

use std::sync::Arc;
use std::time::Duration;

use album_art_core::color::Tuning;
use album_art_core::protocol::Frame;
use log::{info, warn};
use tokio::sync::watch;

use crate::config::Config;
use crate::spotify::{Artwork, SpotifyClient, SpotifyError};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let config = Config::from_env()?;

    let frames_tx = Arc::new(watch::Sender::new(Arc::new(Frame::black())));
    let tuning_tx = Arc::new(watch::Sender::new(Tuning::DEFAULT));
    let spotify = SpotifyClient::new(config.credentials, config.refresh_token);

    tokio::try_join!(
        async {
            follow_spotify(spotify, &frames_tx).await;
            Ok(())
        },
        device::serve(
            config.device_listen,
            frames_tx.subscribe(),
            tuning_tx.subscribe()
        ),
        web::serve(
            config.web_listen,
            Arc::clone(&tuning_tx),
            Arc::clone(&frames_tx)
        ),
    )?;
    Ok(())
}

async fn follow_spotify(mut spotify: SpotifyClient, frames: &watch::Sender<Arc<Frame>>) {
    let mut shown = Artwork::Nothing;
    loop {
        match update(&mut spotify, &shown, frames).await {
            Ok(artwork) => shown = artwork,
            Err(e) => warn!("Spotify: {e:?}"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Publishes a new frame when what is playing changed, and returns what is
/// shown now.
async fn update(
    spotify: &mut SpotifyClient,
    shown: &Artwork,
    frames: &watch::Sender<Arc<Frame>>,
) -> Result<Artwork, UpdateError> {
    let artwork = spotify.now_playing().await?;
    if artwork == *shown {
        return Ok(artwork);
    }
    info!("Now showing {artwork:?}");
    let frame = match &artwork {
        Artwork::Nothing => Frame::black(),
        Artwork::Image(url) => cover::to_frame(&spotify.image(url).await?)?,
    };
    frames.send_replace(Arc::new(frame));
    Ok(artwork)
}

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only read through Debug when logged")]
enum UpdateError {
    Spotify(SpotifyError),
    Image(image::ImageError),
}

impl From<SpotifyError> for UpdateError {
    fn from(e: SpotifyError) -> Self {
        UpdateError::Spotify(e)
    }
}

impl From<image::ImageError> for UpdateError {
    fn from(e: image::ImageError) -> Self {
        UpdateError::Image(e)
    }
}

mod config;
mod device;
mod playback;
mod presets;
mod scale;
mod spotify;
mod web;

use std::sync::Arc;
use std::time::Duration;

use album_art_core::color::Tuning;
use album_art_core::protocol::Frame;
use log::{info, warn};
use tokio::sync::watch;

use crate::config::Config;
use crate::playback::Source;
use crate::spotify::{Artwork, SpotifyClient, SpotifyError};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let config = Config::from_env()?;

    let presets = Arc::new(presets::load(&config.presets_dir)?);
    info!("Loaded presets: {:?}", presets.keys().collect::<Vec<_>>());

    // Spotify publishes covers; playback picks what the panel shows
    let covers = watch::Sender::new(Arc::new(Frame::black()));
    let source = Arc::new(watch::Sender::new(Source::Song));
    let output = Arc::new(watch::Sender::new(Arc::new(Frame::black())));
    let tuning = Arc::new(watch::Sender::new(Tuning::DEFAULT));
    let spotify = SpotifyClient::new(config.credentials, config.refresh_token);

    tokio::try_join!(
        async {
            follow_spotify(spotify, &covers).await;
            Ok(())
        },
        async {
            playback::run(Arc::clone(&source), covers.subscribe(), Arc::clone(&output)).await;
            Ok(())
        },
        device::serve(config.device_listen, output.subscribe(), tuning.subscribe()),
        web::serve(
            config.web_listen,
            Arc::clone(&tuning),
            Arc::clone(&source),
            output.subscribe(),
            presets,
        ),
    )?;
    Ok(())
}

async fn follow_spotify(mut spotify: SpotifyClient, covers: &watch::Sender<Arc<Frame>>) {
    let mut shown = Artwork::Nothing;
    loop {
        match update(&mut spotify, &shown, covers).await {
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
    covers: &watch::Sender<Arc<Frame>>,
) -> Result<Artwork, UpdateError> {
    let artwork = spotify.now_playing().await?;
    if artwork == *shown {
        return Ok(artwork);
    }
    info!("Now showing {artwork:?}");
    let frame = match &artwork {
        Artwork::Nothing => Frame::black(),
        Artwork::Image(url) => scale::to_frame(&spotify.image(url).await?)?,
    };
    covers.send_replace(Arc::new(frame));
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

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
use tokio::time::Instant;

use crate::config::Config;
use crate::playback::Source;
use crate::spotify::{Artwork, NowPlaying, SpotifyClient, SpotifyError};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
// Spotify reports the next track only after the player has moved on to it
const TRACK_END_MARGIN: Duration = Duration::from_millis(250);
// Bounds the polling rate while Spotify still reports a track that should
// have ended
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(500);

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
        let polled_at = Instant::now();
        let remaining = match update(&mut spotify, &shown, covers).await {
            Ok(now_playing) => {
                shown = now_playing.artwork;
                now_playing.remaining
            }
            Err(e) => {
                warn!("Spotify: {e:?}");
                None
            }
        };
        tokio::time::sleep_until(polled_at + poll_delay(remaining)).await;
    }
}

/// Polls again right after the current item ends, so natural track changes
/// show up without waiting for the regular interval.
fn poll_delay(remaining: Option<Duration>) -> Duration {
    let Some(remaining) = remaining else {
        return POLL_INTERVAL;
    };
    (remaining + TRACK_END_MARGIN).clamp(MIN_POLL_INTERVAL, POLL_INTERVAL)
}

/// Publishes a new frame when the artwork changed.
async fn update(
    spotify: &mut SpotifyClient,
    shown: &Artwork,
    covers: &watch::Sender<Arc<Frame>>,
) -> Result<NowPlaying, UpdateError> {
    let now_playing = spotify.now_playing().await?;
    if now_playing.artwork == *shown {
        return Ok(now_playing);
    }
    info!("Now showing {:?}", now_playing.artwork);
    let frame = match &now_playing.artwork {
        Artwork::Nothing => Frame::black(),
        Artwork::Image(url) => scale::to_frame(&spotify.image(url).await?)?,
    };
    covers.send_replace(Arc::new(frame));
    Ok(now_playing)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_playing_polls_at_the_regular_interval() {
        assert_eq!(poll_delay(None), POLL_INTERVAL);
    }

    #[test]
    fn long_track_polls_at_the_regular_interval() {
        assert_eq!(poll_delay(Some(Duration::from_secs(60))), POLL_INTERVAL);
    }

    #[test]
    fn ending_track_polls_just_after_it_ends() {
        assert_eq!(
            poll_delay(Some(Duration::from_millis(800))),
            Duration::from_millis(800) + TRACK_END_MARGIN
        );
    }

    #[test]
    fn overdue_track_does_not_poll_faster_than_the_minimum() {
        assert_eq!(poll_delay(Some(Duration::ZERO)), MIN_POLL_INTERVAL);
    }
}

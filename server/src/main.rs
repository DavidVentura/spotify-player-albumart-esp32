mod color;
mod config;
mod device;
mod frame;
mod playback;
mod presets;
mod scale;
mod spotify;
mod web;

use std::sync::Arc;
use std::time::Duration;

use log::{info, warn};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::color::Tuning;
use crate::config::Config;
use crate::frame::Frame;
use crate::playback::Source;
use crate::spotify::{Artwork, ImageUrl, NowPlaying, Silence, SpotifyClient, SpotifyError};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
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
    let mut heard = None;
    let mut shown = Shown::Dark;
    loop {
        let polled_at = Instant::now();
        let delay = match update(&mut spotify, &shown, polled_at, covers).await {
            Ok((now_playing, next)) => {
                if heard.as_ref() != Some(&now_playing.artwork) {
                    info!("Spotify reports {:?}", now_playing.artwork);
                }
                heard = Some(now_playing.artwork);
                shown = next;
                poll_delay(now_playing.remaining)
            }
            // Polling on through a rate limit keeps extending it
            Err(UpdateError::Spotify(SpotifyError::RateLimited { retry_after })) => {
                warn!("Spotify: rate limited for {}s", retry_after.as_secs());
                retry_after.max(POLL_INTERVAL)
            }
            Err(e) => {
                warn!("Spotify: {e:?}");
                POLL_INTERVAL
            }
        };
        tokio::time::sleep_until(polled_at + delay).await;
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

/// The Spotify cover on the panel, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Shown {
    Dark,
    Cover {
        url: ImageUrl,
        /// When its track should end, if Spotify said how far in it was.
        ends: Option<Instant>,
    },
}

impl Shown {
    /// Spotify can report silence for a while mid-track, such as while a
    /// player reconnects, so a silent reply that names no other item keeps the
    /// cover until its track would have ended.
    fn after(&self, now_playing: &NowPlaying, polled_at: Instant) -> Shown {
        let silence = match &now_playing.artwork {
            Artwork::Image(url) => {
                return Shown::Cover {
                    url: url.clone(),
                    ends: now_playing.remaining.map(|remaining| polled_at + remaining),
                };
            }
            Artwork::Nothing(silence) => silence,
        };
        match (silence, self) {
            (Silence::NoImages, _) => Shown::Dark,
            (
                _,
                Shown::Cover {
                    ends: Some(ends), ..
                },
            ) if polled_at < *ends => self.clone(),
            _ => Shown::Dark,
        }
    }

    fn cover(&self) -> Option<&ImageUrl> {
        match self {
            Shown::Dark => None,
            Shown::Cover { url, .. } => Some(url),
        }
    }
}

/// Publishes a new frame when the cover to show changed.
async fn update(
    spotify: &mut SpotifyClient,
    shown: &Shown,
    polled_at: Instant,
    covers: &watch::Sender<Arc<Frame>>,
) -> Result<(NowPlaying, Shown), UpdateError> {
    let now_playing = spotify.now_playing().await?;
    let next = shown.after(&now_playing, polled_at);
    if next.cover() == shown.cover() {
        return Ok((now_playing, next));
    }
    info!("Now showing {:?}", next.cover());
    let frame = match next.cover() {
        None => Frame::black(),
        Some(url) => scale::to_frame(&spotify.image(url).await?)?,
    };
    covers.send_replace(Arc::new(frame));
    Ok((now_playing, next))
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

    fn cover(url: &str, ends: Option<Instant>) -> Shown {
        Shown::Cover {
            url: ImageUrl::parse(url).unwrap(),
            ends,
        }
    }

    fn playing(url: &str, remaining: Duration) -> NowPlaying {
        NowPlaying {
            artwork: Artwork::Image(ImageUrl::parse(url).unwrap()),
            remaining: Some(remaining),
        }
    }

    fn silent(silence: Silence) -> NowPlaying {
        NowPlaying {
            artwork: Artwork::Nothing(silence),
            remaining: None,
        }
    }

    const A: &str = "https://i.scdn.co/image/a";
    const B: &str = "https://i.scdn.co/image/b";

    #[test]
    fn playing_track_shows_its_cover_until_it_ends() {
        let now = Instant::now();
        assert_eq!(
            Shown::Dark.after(&playing(A, Duration::from_secs(60)), now),
            cover(A, Some(now + Duration::from_secs(60)))
        );
    }

    #[test]
    fn silence_mid_track_keeps_the_cover() {
        let now = Instant::now();
        let shown = cover(A, Some(now + Duration::from_secs(60)));
        for silence in [Silence::NoPlayer, Silence::Paused, Silence::NoItem] {
            assert_eq!(shown.after(&silent(silence), now), shown);
        }
    }

    #[test]
    fn silence_after_the_track_ends_goes_dark() {
        let now = Instant::now();
        let shown = cover(A, Some(now));
        assert_eq!(shown.after(&silent(Silence::Paused), now), Shown::Dark);
    }

    #[test]
    fn silence_without_a_known_end_goes_dark() {
        let shown = cover(A, None);
        assert_eq!(
            shown.after(&silent(Silence::NoPlayer), Instant::now()),
            Shown::Dark
        );
    }

    #[test]
    fn item_without_art_mid_track_goes_dark() {
        let now = Instant::now();
        let shown = cover(A, Some(now + Duration::from_secs(60)));
        assert_eq!(shown.after(&silent(Silence::NoImages), now), Shown::Dark);
    }

    #[test]
    fn other_track_mid_track_replaces_the_cover() {
        let now = Instant::now();
        let shown = cover(A, Some(now + Duration::from_secs(60)));
        assert_eq!(
            shown.after(&playing(B, Duration::from_secs(10)), now),
            cover(B, Some(now + Duration::from_secs(10)))
        );
    }

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

//! The panel connects here and is pushed the latest frame and tuning.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use album_art_core::protocol::{Brightness, Message};
use log::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::color::{ColorCorrection, Tuning};
use crate::frame::Frame;

const STATS_INTERVAL: Duration = Duration::from_secs(5);

pub async fn serve(
    address: SocketAddr,
    frames: watch::Receiver<Arc<Frame>>,
    tuning: watch::Receiver<Tuning>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(address).await?;
    info!("Device: listening on {address}");
    loop {
        let (stream, peer) = listener.accept().await?;
        info!("Device: {peer} connected");
        let frames = frames.clone();
        let tuning = tuning.clone();
        tokio::spawn(async move {
            match push(stream, peer, frames, tuning).await {
                Ok(()) => info!("Device: {peer} disconnected"),
                Err(e) => warn!("Device: {peer}: {e}"),
            }
        });
    }
}

/// Sends the current state, then every change. A `watch` only holds the
/// newest value, so a slow panel skips stale frames instead of falling
/// behind. Colour settings are applied here, so changing them re-sends the
/// frame on screen; brightness is the panel's own and goes over as is.
async fn push(
    mut stream: TcpStream,
    peer: SocketAddr,
    mut frames: watch::Receiver<Arc<Frame>>,
    mut tuning: watch::Receiver<Tuning>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut current = *tuning.borrow_and_update();
    let mut correction = ColorCorrection::new(current.gamma, current.white_balance);
    stream
        .write_all(&Message::Brightness(current.brightness).encode())
        .await?;
    frames.mark_changed();
    let mut window_start = Instant::now();
    let mut frames_sent = 0u32;
    loop {
        tokio::select! {
            biased;
            changed = tuning.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let next = *tuning.borrow_and_update();
                let change = TuningChange::between(current, next);
                current = next;
                if let Some(brightness) = change.brightness {
                    stream.write_all(&Message::Brightness(brightness).encode()).await?;
                }
                if change.colour {
                    correction = ColorCorrection::new(current.gamma, current.white_balance);
                    frames.mark_changed();
                }
            }
            changed = frames.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let frame = Arc::clone(&frames.borrow_and_update());
                let panel = correction.apply(&frame);
                stream.write_all(&Message::Frame(panel).encode()).await?;
                frames_sent += 1;
            }
        }
        if window_start.elapsed() >= STATS_INTERVAL {
            let seconds = window_start.elapsed().as_secs_f32();
            info!("Device: {peer}: sent {frames_sent} frames in {seconds:.1}s");
            window_start = Instant::now();
            frames_sent = 0;
        }
    }
}

/// What the panel needs to hear about a tuning update.
#[derive(Debug, PartialEq, Eq)]
struct TuningChange {
    brightness: Option<Brightness>,
    /// Gamma or white balance changed, so the frame must be corrected again.
    colour: bool,
}

impl TuningChange {
    fn between(old: Tuning, new: Tuning) -> Self {
        TuningChange {
            brightness: (old.brightness != new.brightness).then_some(new.brightness),
            colour: old.gamma != new.gamma || old.white_balance != new.white_balance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{GammaExponent, WhiteBalance};

    #[test]
    fn brightness_alone_does_not_resend_the_frame() {
        let dimmer = Tuning {
            brightness: Brightness::new(5).unwrap(),
            ..Tuning::DEFAULT
        };
        assert_eq!(
            TuningChange::between(Tuning::DEFAULT, dimmer),
            TuningChange {
                brightness: Some(Brightness::new(5).unwrap()),
                colour: false,
            }
        );
    }

    #[test]
    fn colour_settings_resend_the_frame() {
        let warmer = Tuning {
            gamma: GammaExponent::from_hundredths(250).unwrap(),
            white_balance: WhiteBalance::new(100, 90, 70).unwrap(),
            ..Tuning::DEFAULT
        };
        assert_eq!(
            TuningChange::between(Tuning::DEFAULT, warmer),
            TuningChange {
                brightness: None,
                colour: true,
            }
        );
    }

    #[test]
    fn unchanged_tuning_sends_nothing() {
        assert_eq!(
            TuningChange::between(Tuning::DEFAULT, Tuning::DEFAULT),
            TuningChange {
                brightness: None,
                colour: false,
            }
        );
    }
}

//! The panel connects here and is pushed the latest frame and tuning.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use album_art_core::protocol::{Brightness, Depth, Message, REPORT_LEN, Report};
use log::{info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::color::{ColorCorrection, Tuning};
use crate::frame::Frame;

const STATS_INTERVAL: Duration = Duration::from_secs(5);
/// Bytes the kernel may hold unsent before writes block. Small, so frames
/// that pile up while the panel is busy are replaced in the `watch` instead of
/// queueing in the socket and playing back late.
const UNSENT_LIMIT: u32 = 1024;

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
/// behind. Colour settings are applied here; brightness is applied by the
/// panel as it shows a frame, and it keeps no copy to redraw from. It
/// answers every brightness with the depth that brightness leaves it, and
/// each answer re-sends the frame on screen quantized to that depth; frames
/// wait for the first one.
async fn push(
    stream: TcpStream,
    peer: SocketAddr,
    frames: watch::Receiver<Arc<Frame>>,
    tuning: watch::Receiver<Tuning>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    socket2::SockRef::from(&stream).set_tcp_notsent_lowat(UNSENT_LIMIT)?;
    let (reader, stream) = stream.into_split();
    let (depths, depth_reports) = watch::channel(None);
    // Whichever side ends first ends the connection
    tokio::select! {
        result = read_reports(reader, &depths) => result,
        result = send_updates(stream, peer, frames, tuning, depth_reports) => result,
    }
}

async fn send_updates(
    mut stream: OwnedWriteHalf,
    peer: SocketAddr,
    mut frames: watch::Receiver<Arc<Frame>>,
    mut tuning: watch::Receiver<Tuning>,
    mut depth_reports: watch::Receiver<Option<Depth>>,
) -> std::io::Result<()> {
    let mut depth: Option<Depth> = None;
    let mut current = *tuning.borrow_and_update();
    let mut correction = ColorCorrection::new(current.gamma, current.white_balance);
    stream
        .write_all(&Message::Brightness(current.brightness).encode())
        .await?;
    let mut window_start = Instant::now();
    let mut frames_sent = 0u32;
    loop {
        tokio::select! {
            biased;
            changed = depth_reports.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                depth = *depth_reports.borrow_and_update();
                frames.mark_changed();
            }
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
            changed = frames.changed(), if depth.is_some() => {
                if changed.is_err() {
                    return Ok(());
                }
                let frame = Arc::clone(&frames.borrow_and_update());
                let depth = depth.expect("frames wait for the panel's depth");
                let panel = correction.apply(&frame, depth);
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

/// Publishes each depth the panel reports, until it disconnects.
///
/// Cancel-safe only between reports, which is fine: it is dropped only
/// when the connection ends.
async fn read_reports(
    mut reader: OwnedReadHalf,
    depths: &watch::Sender<Option<Depth>>,
) -> std::io::Result<()> {
    loop {
        let mut bytes = [0; REPORT_LEN];
        match reader.read_exact(&mut bytes).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let Report::Depth(depth) = Report::decode(bytes).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}"))
        })?;
        info!("Device: panel shows {} bits per channel", depth.bits());
        // Sent even when unchanged: every report follows a brightness the
        // panel has applied, and the frame on screen needs re-sending
        depths.send_replace(Some(depth));
    }
}

/// What the panel needs to hear about a tuning update.
#[derive(Debug, PartialEq, Eq)]
struct TuningChange {
    brightness: Option<Brightness>,
    /// Gamma or white balance changed, so the correction must be rebuilt.
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
    fn brightness_alone_keeps_the_colour_correction() {
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

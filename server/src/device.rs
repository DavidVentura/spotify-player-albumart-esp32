//! The panel connects here and is pushed the latest frame and tuning.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use album_art_core::color::Tuning;
use album_art_core::protocol::{Frame, Message};
use log::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

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
/// behind.
async fn push(
    mut stream: TcpStream,
    peer: SocketAddr,
    mut frames: watch::Receiver<Arc<Frame>>,
    mut tuning: watch::Receiver<Tuning>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    frames.mark_changed();
    tuning.mark_changed();
    let mut window_start = Instant::now();
    let mut frames_sent = 0u32;
    loop {
        let message = tokio::select! {
            // Tuning first, so a new connection renders its first frame with
            // the right settings
            biased;
            changed = tuning.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                Message::Tune(*tuning.borrow_and_update())
            }
            changed = frames.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let frame = Frame::clone(&frames.borrow_and_update());
                frames_sent += 1;
                Message::Frame(frame)
            }
        };
        stream.write_all(&message.encode()).await?;
        if window_start.elapsed() >= STATS_INTERVAL {
            let seconds = window_start.elapsed().as_secs_f32();
            info!("Device: {peer}: sent {frames_sent} frames in {seconds:.1}s");
            window_start = Instant::now();
            frames_sent = 0;
        }
    }
}

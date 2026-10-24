//! The panel connects here and is pushed the latest frame and tuning.

use std::net::SocketAddr;
use std::sync::Arc;

use album_art_core::color::Tuning;
use album_art_core::protocol::{Frame, Message};
use log::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

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
            match push(stream, frames, tuning).await {
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
    mut frames: watch::Receiver<Arc<Frame>>,
    mut tuning: watch::Receiver<Tuning>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    frames.mark_changed();
    tuning.mark_changed();
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
                Message::Frame(frame)
            }
        };
        stream.write_all(&message.encode()).await?;
    }
}

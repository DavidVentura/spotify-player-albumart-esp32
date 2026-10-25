use core::net::SocketAddrV4;
use core::sync::atomic::{AtomicU32, Ordering};

use album_art_core::protocol::{ProtocolError, StreamDecoder};
use embassy_net::Stack;
use embassy_net::tcp::{self, TcpSocket};
use embassy_time::{Duration, Timer};
use log::{info, warn};

use crate::display::Panel;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// A full frame is 12 KiB; a large window keeps it to a few round trips.
const RX_BUFFER: usize = 8 * 1024;
const READ_CHUNK: usize = 1024;
/// Without traffic from the server, keepalives notice a server that vanished
/// without closing the connection.
const KEEP_ALIVE: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(30);

pub static RECEIVED_BYTES: AtomicU32 = AtomicU32::new(0);

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only read through Debug when logged")]
enum LinkError {
    Connect(tcp::ConnectError),
    Read(tcp::Error),
    Closed,
    Protocol(ProtocolError),
}

/// Keeps a connection to the server open and decodes everything it sends
/// straight onto the panel. The last frame stays up while reconnecting.
#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, server: SocketAddrV4, mut panel: Panel) {
    let mut rx_buffer = alloc::vec![0; RX_BUFFER];
    let mut tx_buffer = [0; 64];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_keep_alive(Some(KEEP_ALIVE));
        socket.set_timeout(Some(TIMEOUT));
        match receive(&mut socket, server, &mut panel).await {
            Ok(()) => {}
            Err(e) => warn!("Link: {e:?}"),
        }
        socket.abort();
        Timer::after(RECONNECT_DELAY).await;
    }
}

async fn receive(
    socket: &mut TcpSocket<'_>,
    server: SocketAddrV4,
    panel: &mut Panel,
) -> Result<(), LinkError> {
    socket.connect(server).await.map_err(LinkError::Connect)?;
    info!("Link: connected to {server}");
    // A frame cut off by a dropped connection stays half-written in the back
    // buffer; the next one overwrites every pixel before it is shown
    let mut decoder = StreamDecoder::default();
    let mut chunk = [0; READ_CHUNK];
    loop {
        let read = socket.read(&mut chunk).await.map_err(LinkError::Read)?;
        if read == 0 {
            return Err(LinkError::Closed);
        }
        RECEIVED_BYTES.fetch_add(read as u32, Ordering::Relaxed);
        let mut rest = &chunk[..read];
        while !rest.is_empty() {
            let progress = decoder.feed(rest, panel).map_err(LinkError::Protocol)?;
            if progress.frame_complete {
                panel.present().await;
            }
            rest = &rest[progress.consumed..];
        }
    }
}

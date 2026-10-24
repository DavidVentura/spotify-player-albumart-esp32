use album_art_core::control::{Command, Line, LineSplitter};
use embassy_net::Stack;
use embassy_net::tcp::{self, TcpSocket};
use embassy_time::Duration;
use log::{info, warn};

use crate::display;

const PORT: u16 = 7777;
/// `nc` without `-N`/`-q0` keeps the connection open after its input ends;
/// close idle clients so the next one can connect.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Accepts one client at a time and applies every line that is a valid
/// [`Command`]; other lines are ignored.
#[embassy_executor::task]
pub async fn server(stack: Stack<'static>) {
    let mut rx_buffer = [0; 128];
    let mut tx_buffer = [0; 16];
    info!("Control: listening on port {PORT}");
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(IDLE_TIMEOUT));
        if let Err(e) = socket.accept(PORT).await {
            warn!("Control: accept failed: {e:?}");
            continue;
        }
        serve(&mut socket).await;
    }
}

async fn serve(socket: &mut TcpSocket<'_>) {
    let mut lines = LineSplitter::default();
    let mut chunk = [0; 32];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                for line in lines.feed(&chunk[..read]) {
                    apply(line).await;
                }
            }
            // Also how an idle timeout surfaces
            Err(tcp::Error::ConnectionReset) => return,
        }
    }
    if let Some(line) = lines.finish() {
        apply(line).await;
    }
    socket.close();
    // The client may already be gone; nothing is left to deliver either way
    socket.flush().await.ok();
}

async fn apply(line: Line) {
    let Line::Text(text) = line else {
        return;
    };
    if let Some(command) = Command::parse(&text) {
        display::adjust(command).await;
    }
}

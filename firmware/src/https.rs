use alloc::vec;
use alloc::vec::Vec;
use core::ffi::CStr;

use album_art_core::http::{HttpError, Parse, Request, Response, Stream, parse_response};
use embassy_net::Stack;
use embassy_net::dns::{self, DnsQueryType};
use embassy_net::tcp::{self, TcpSocket};
use embassy_time::Duration;
use log::warn;
use mbedtls_rs::io::Write;
use mbedtls_rs::{
    Certificate, ClientSessionConfig, Session, SessionConfig, SessionError, Tls, X509,
};

/// Roots of every chain Spotify's API, accounts and image hosts serve.
const ROOTS: &CStr = match CStr::from_bytes_with_nul(
    concat!(include_str!("../certs/spotify-roots.pem"), "\0").as_bytes(),
) {
    Ok(roots) => roots,
    Err(_) => panic!("root bundle contains a NUL byte"),
};

const HTTPS_PORT: u16 = 443;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_RX_BUFFER: usize = 4096;
const TCP_TX_BUFFER: usize = 2048;
const READ_CHUNK: usize = 1024;
/// A currently-playing response is a few KiB and a 64px cover is smaller.
const MAX_RESPONSE: usize = 32 * 1024;

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only read through Debug when logged")]
pub enum HttpsError {
    Dns(dns::Error),
    NoAddress,
    Connect(tcp::ConnectError),
    Tls(SessionError),
    Http(HttpError),
    ResponseTooLarge,
}

pub struct HttpsClient<'a> {
    stack: Stack<'a>,
    tls: &'a Tls<'a>,
    roots: Certificate<'static>,
}

impl<'a> HttpsClient<'a> {
    pub fn new(stack: Stack<'a>, tls: &'a Tls<'a>) -> Self {
        let roots = Certificate::new(X509::PEM(ROOTS)).expect("bundled root certificates parse");
        HttpsClient { stack, tls, roots }
    }

    /// Sends one request on a fresh connection and reads the full response.
    pub async fn send(&self, request: &Request) -> Result<Response, HttpsError> {
        let host = request.host.name();
        let address = *self
            .stack
            .dns_query(
                host.to_str().expect("host names are ASCII"),
                DnsQueryType::A,
            )
            .await
            .map_err(HttpsError::Dns)?
            .first()
            .ok_or(HttpsError::NoAddress)?;

        let mut rx_buffer = vec![0; TCP_RX_BUFFER];
        let mut tx_buffer = vec![0; TCP_TX_BUFFER];
        let mut socket = TcpSocket::new(self.stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(SOCKET_TIMEOUT));
        socket
            .connect((address, HTTPS_PORT))
            .await
            .map_err(HttpsError::Connect)?;

        let config = SessionConfig::Client(ClientSessionConfig {
            ca_chain: Some(self.roots.clone()),
            server_name: Some(host),
            ..ClientSessionConfig::new()
        });
        let mut session =
            Session::new(self.tls.reference(), socket, &config).map_err(HttpsError::Tls)?;
        session
            .write_all(&request.encode())
            .await
            .map_err(HttpsError::Tls)?;
        session.flush().await.map_err(HttpsError::Tls)?;

        let mut raw = Vec::new();
        let mut chunk = [0; READ_CHUNK];
        loop {
            let read = session.read(&mut chunk).await.map_err(HttpsError::Tls)?;
            let stream = match read {
                0 => Stream::Closed,
                _ => Stream::Open,
            };
            raw.extend_from_slice(&chunk[..read]);
            if raw.len() > MAX_RESPONSE {
                return Err(HttpsError::ResponseTooLarge);
            }
            match parse_response(&raw, stream).map_err(HttpsError::Http)? {
                Parse::Complete(response) => {
                    // The response is already complete, so a failed close_notify
                    // does not invalidate it
                    if let Err(e) = session.close().await {
                        warn!("HTTPS: closing {host:?} failed: {e:?}");
                    }
                    return Ok(response);
                }
                Parse::NeedMore => {}
            }
        }
    }
}

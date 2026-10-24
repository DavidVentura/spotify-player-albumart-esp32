use core::net::SocketAddr;

use embassy_net::Stack;
use embassy_net::dns::{self, DnsQueryType};
use embassy_net::udp::{self, PacketMetadata, UdpSocket};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use esp_hal::rtc_cntl::Rtc;
use log::{info, warn};
use sntpc::{NtpContext, NtpTimestampGenerator};
use sntpc_net_embassy::UdpSocketWrapper;

const NTP_SERVER: &str = "pool.ntp.org";
const NTP_PORT: u16 = 123;
const LOCAL_PORT: u16 = 50123;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only read through Debug when logged")]
enum ClockError {
    Dns(dns::Error),
    NoAddress,
    Bind(udp::BindError),
    Timeout,
    Sntp(sntpc::Error),
}

/// Sets the RTC to the current UNIX time; certificate validation reads it.
pub async fn sync(stack: Stack<'_>, rtc: &Rtc<'_>) {
    loop {
        match query(stack).await {
            Ok(unix_micros) => {
                rtc.set_current_time_us(unix_micros);
                info!("Clock: synced to {} s", unix_micros / 1_000_000);
                return;
            }
            Err(e) => warn!("Clock: SNTP failed: {e:?}"),
        }
        Timer::after(RETRY_DELAY).await;
    }
}

async fn query(stack: Stack<'_>) -> Result<u64, ClockError> {
    let server = *stack
        .dns_query(NTP_SERVER, DnsQueryType::A)
        .await
        .map_err(ClockError::Dns)?
        .first()
        .ok_or(ClockError::NoAddress)?;

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx_buffer = [0; 128];
    let mut tx_buffer = [0; 128];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    socket.bind(LOCAL_PORT).map_err(ClockError::Bind)?;
    let socket = UdpSocketWrapper::new(socket);

    let result = sntpc::get_time(
        SocketAddr::new(server.into(), NTP_PORT),
        &socket,
        NtpContext::new(UptimeClock::default()),
    )
    .with_timeout(QUERY_TIMEOUT)
    .await
    .map_err(|_| ClockError::Timeout)?
    .map_err(ClockError::Sntp)?;

    Ok(
        result.seconds * 1_000_000
            + sntpc::fraction_to_microseconds(result.seconds_fraction) as u64,
    )
}

/// sntpc only uses the local clock to measure the round trip, so time since
/// boot is enough.
#[derive(Clone, Copy, Default)]
struct UptimeClock {
    micros: u64,
}

impl NtpTimestampGenerator for UptimeClock {
    fn init(&mut self) {
        self.micros = Instant::now().as_micros();
    }

    fn timestamp_sec(&self) -> u64 {
        self.micros / 1_000_000
    }

    fn timestamp_subsec_micros(&self) -> u32 {
        (self.micros % 1_000_000) as u32
    }
}

#![no_std]
#![no_main]

extern crate alloc;

mod clock;
mod config;
mod control;
mod display;
mod https;
mod wifi;

use album_art_core::spotify::{
    self, AccessToken, Artwork, ClientAuth, ImageUrl, RefreshToken, SpotifyError,
};
use album_art_core::thumbnail::{self, DecodeError, Thumbnail};
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Pin;
use esp_hal::ram;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::rtc_cntl::Rtc;
use esp_hal::timer::timg::TimerGroup;
use esp_hub75::Hub75Pins16;
use esp_metadata_generated::memory_range;
use log::{info, warn};
use mbedtls_rs::Tls;
use mbedtls_rs::sys::hook::backend::embassy::timer::EmbassyTimer;
use mbedtls_rs::sys::hook::backend::esp::EspAccel;
use mbedtls_rs::sys::hook::backend::esp::wall_clock::EspRtcWallClock;
use static_cell::StaticCell;
use tinyrlibc as _;

use crate::display::Screen;
use crate::https::{HttpsClient, HttpsError};

esp_bootloader_esp_idf::esp_app_desc!();

const RECLAIMED_RAM: usize =
    memory_range!("DRAM2_UNINIT").end - memory_range!("DRAM2_UNINIT").start;
const MAIN_HEAP: usize = 72 * 1024;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// The RTC drifts by minutes per day, which matters for short-lived certificates.
const CLOCK_RESYNC_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    heap_allocator!(#[ram(reclaimed)] size: RECLAIMED_RAM);
    heap_allocator!(size: MAIN_HEAP);

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    display::start(
        spawner,
        peripherals.I2S0,
        peripherals.DMA_I2S0,
        Hub75Pins16 {
            red1: peripherals.GPIO2.degrade(),
            grn1: peripherals.GPIO4.degrade(),
            blu1: peripherals.GPIO5.degrade(),
            red2: peripherals.GPIO18.degrade(),
            grn2: peripherals.GPIO19.degrade(),
            blu2: peripherals.GPIO21.degrade(),
            addr0: peripherals.GPIO12.degrade(),
            addr1: peripherals.GPIO13.degrade(),
            addr2: peripherals.GPIO14.degrade(),
            addr3: peripherals.GPIO15.degrade(),
            addr4: peripherals.GPIO22.degrade(),
            blank: peripherals.GPIO27.degrade(),
            clock: peripherals.GPIO25.degrade(),
            latch: peripherals.GPIO26.degrade(),
        },
    );

    let _trng_source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    static TRNG: StaticCell<Trng> = StaticCell::new();
    let trng = TRNG.init(Trng::try_new().expect("TRNG source is running"));
    let seed = (u64::from(trng.random()) << 32) | u64::from(trng.random());

    let stack = wifi::connect(spawner, peripherals.WIFI, seed).await;
    spawner.spawn(control::server(stack).expect("control server is spawned once"));

    static RTC: StaticCell<Rtc> = StaticCell::new();
    let rtc: &'static Rtc = RTC.init(Rtc::new(peripherals.RTC_TIMER));
    clock::sync(stack, rtc).await;

    static TIMER: EmbassyTimer = EmbassyTimer;
    static WALL_CLOCK: StaticCell<EspRtcWallClock<&Rtc>> = StaticCell::new();
    let wall_clock: &'static EspRtcWallClock<&Rtc> = WALL_CLOCK.init(EspRtcWallClock::new(rtc));
    // SAFETY: both hooks are 'static and installed before any TLS session exists
    unsafe {
        mbedtls_rs::sys::hook::timer::hook_timer(Some(&TIMER));
        mbedtls_rs::sys::hook::wall_clock::hook_wall_clock(Some(wall_clock));
    }
    let mut accel = EspAccel::new()
        .with_rsa(peripherals.RSA)
        .with_aes(peripherals.AES);
    let accel_queue = accel.start();
    // SAFETY: the queue servicing the hooked algorithms lives as long as main,
    // and no MbedTLS state exists yet
    let _hooks = unsafe { accel_queue.hook() };

    let tls = Tls::new(trng).expect("only one TLS instance exists");
    let mut spotify = Spotify {
        https: HttpsClient::new(stack, &tls),
        client: ClientAuth::new(config::SPOTIFY_CLIENT_ID, config::SPOTIFY_CLIENT_SECRET),
        refresh_token: RefreshToken::new(config::SPOTIFY_REFRESH_TOKEN.into()),
        access: None,
    };

    let mut shown = Artwork::Nothing;
    let mut last_clock_sync = Instant::now();
    loop {
        if last_clock_sync.elapsed() >= CLOCK_RESYNC_INTERVAL {
            clock::sync(stack, rtc).await;
            last_clock_sync = Instant::now();
        }
        match update_display(&mut spotify, &shown).await {
            Ok(artwork) => shown = artwork,
            Err(e) => warn!("Poll failed: {e:?}"),
        }
        Timer::after(POLL_INTERVAL).await;
    }
}

/// Shows whatever is playing now and returns it.
async fn update_display(spotify: &mut Spotify<'_>, shown: &Artwork) -> Result<Artwork, PollError> {
    let artwork = spotify.now_playing().await?;
    if artwork == *shown {
        return Ok(artwork);
    }
    info!("Now showing {artwork:?}");
    let screen = match &artwork {
        Artwork::Nothing => Screen::Blank,
        Artwork::Image(url) => Screen::Cover(spotify.thumbnail(url).await?),
    };
    display::show(screen);
    Ok(artwork)
}

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only read through Debug when logged")]
enum PollError {
    Https(HttpsError),
    Spotify(SpotifyError),
    Decode(DecodeError),
}

impl From<HttpsError> for PollError {
    fn from(e: HttpsError) -> Self {
        PollError::Https(e)
    }
}

impl From<SpotifyError> for PollError {
    fn from(e: SpotifyError) -> Self {
        PollError::Spotify(e)
    }
}

struct Access {
    token: AccessToken,
    expires_at: Instant,
}

struct Spotify<'a> {
    https: HttpsClient<'a>,
    client: ClientAuth,
    refresh_token: RefreshToken,
    access: Option<Access>,
}

impl Spotify<'_> {
    async fn now_playing(&mut self) -> Result<Artwork, PollError> {
        let token = self.access_token().await?;
        let response = self
            .https
            .send(&spotify::now_playing_request(&token))
            .await?;
        match spotify::parse_now_playing(&response) {
            Err(SpotifyError::Unauthorized) => {
                self.access = None;
                Err(SpotifyError::Unauthorized.into())
            }
            result => Ok(result?),
        }
    }

    async fn thumbnail(&self, url: &ImageUrl) -> Result<Thumbnail, PollError> {
        let response = self.https.send(&spotify::image_request(url)).await?;
        let jpeg = spotify::parse_image_response(response)?;
        decode(&jpeg).map_err(PollError::Decode)
    }

    async fn access_token(&mut self) -> Result<AccessToken, PollError> {
        if let Some(access) = &self.access
            && Instant::now() < access.expires_at
        {
            return Ok(access.token.clone());
        }
        let request = spotify::refresh_request(&self.client, &self.refresh_token);
        let grant = spotify::parse_token_response(&self.https.send(&request).await?)?;
        if let Some(rotated) = grant.rotated_refresh_token {
            self.refresh_token = rotated;
        }
        let expires_at =
            Instant::now() + Duration::from_secs(grant.expires_in.as_secs()) - TOKEN_EXPIRY_MARGIN;
        self.access = Some(Access {
            token: grant.access_token.clone(),
            expires_at,
        });
        Ok(grant.access_token)
    }
}

/// zune-jpeg's header parsing uses stack frames of up to ~10 KiB. Inlined into
/// the async main task they would stay reserved for the task's whole life and
/// leave no room for the TLS handshake.
#[inline(never)]
fn decode(jpeg: &[u8]) -> Result<Thumbnail, DecodeError> {
    thumbnail::decode_thumbnail(jpeg)
}

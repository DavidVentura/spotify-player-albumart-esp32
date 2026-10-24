use core::sync::atomic::{AtomicU32, Ordering};

use album_art_core::protocol::{Brightness, PANEL_BITS, PanelFrame, SIZE};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::geometry::Point;
use esp_hal::peripherals::{DMA_I2S0, I2S0};
use esp_hal::time::Rate;
use esp_hub75::framebuffer::bitplane::plain::DmaFrameBuffer;
use esp_hub75::framebuffer::compute_rows;
use esp_hub75::{Color, Hub75, Hub75Config, Hub75Pins16};
use log::info;
use static_cell::ConstStaticCell;

const NROWS: usize = compute_rows(SIZE);
const PLANES: usize = PANEL_BITS as usize;
/// The ESP32's I2S peripheral tops out at 19 MHz.
const PIXEL_CLOCK: Rate = Rate::from_mhz(19);

type FrameBuffer = DmaFrameBuffer<NROWS, SIZE, PLANES>;

const REFRESH_HZ: u32 = esp_hub75::refresh_hz::<FrameBuffer>(PIXEL_CLOCK);
// Below this, the panel visibly flickers in peripheral vision
const _: () = assert!(REFRESH_HZ >= 120);

const STATS_INTERVAL: Duration = Duration::from_secs(5);

static FRAME: Signal<CriticalSectionRawMutex, PanelFrame> = Signal::new();
static BRIGHTNESS: Signal<CriticalSectionRawMutex, Brightness> = Signal::new();

// A frame that arrives while another is pending replaces it, so received and
// drawn diverge when the panel cannot keep up
static RECEIVED: AtomicU32 = AtomicU32::new(0);
static DRAWN: AtomicU32 = AtomicU32::new(0);
static RENDER_MICROS: AtomicU32 = AtomicU32::new(0);
static SWAP_MICROS: AtomicU32 = AtomicU32::new(0);

pub fn show(frame: PanelFrame) {
    RECEIVED.fetch_add(1, Ordering::Relaxed);
    FRAME.signal(frame);
}

pub fn set_brightness(brightness: Brightness) {
    BRIGHTNESS.signal(brightness);
}

pub fn start(
    spawner: Spawner,
    i2s: I2S0<'static>,
    dma: DMA_I2S0<'static>,
    pins: Hub75Pins16<'static>,
) {
    // Each buffer is larger than the main stack, so constructing one at
    // runtime would overflow it; zeroed statics formatted in place also keep
    // a formatted copy of both out of the firmware image
    static FRONT: ConstStaticCell<FrameBuffer> = ConstStaticCell::new(FrameBuffer::unformatted());
    static BACK: ConstStaticCell<FrameBuffer> = ConstStaticCell::new(FrameBuffer::unformatted());
    let front = FRONT.take();
    let back = BACK.take();
    front.format();
    back.format();

    let hub75 = Hub75::new_async(
        i2s,
        pins,
        dma,
        esp_hub75::hub75_dma_descriptors!(FrameBuffer),
        Hub75Config::new().with_frequency(PIXEL_CLOCK),
        &*front,
    )
    .expect("HUB75 driver starts");
    info!("Display: refreshing at {REFRESH_HZ} Hz");

    spawner.spawn(display_task(hub75, back).expect("display task is spawned once"));
    spawner.spawn(stats_task().expect("stats task is spawned once"));
}

/// Logs throughput while frames are arriving, and stays quiet otherwise.
#[embassy_executor::task]
async fn stats_task() {
    loop {
        Timer::after(STATS_INTERVAL).await;
        let received = RECEIVED.swap(0, Ordering::Relaxed);
        let drawn = DRAWN.swap(0, Ordering::Relaxed);
        let render = RENDER_MICROS.swap(0, Ordering::Relaxed);
        let swap = SWAP_MICROS.swap(0, Ordering::Relaxed);
        if received == 0 {
            continue;
        }
        let per_draw = |micros: u32| micros / drawn.max(1);
        info!(
            "Display: {received} frames received, {drawn} drawn in {}s; per draw {}us rendering, {}us waiting for the swap",
            STATS_INTERVAL.as_secs(),
            per_draw(render),
            per_draw(swap),
        );
    }
}

#[embassy_executor::task]
async fn display_task(
    hub75: Hub75<esp_hal::Async, FrameBuffer>,
    mut back: &'static mut FrameBuffer,
) {
    let mut frame = PanelFrame::black();
    // The server sends brightness first; until then nothing is lit
    let mut brightness = Brightness::OFF;
    loop {
        match select(FRAME.wait(), BRIGHTNESS.wait()).await {
            Either::First(next) => frame = next,
            Either::Second(next) => {
                brightness = next;
                info!("Display: brightness {}%", brightness.percent());
            }
        }
        let render_start = Instant::now();
        back.erase();
        // The framebuffer takes 8-bit channels and shows their top bits
        let shift = 8 - PANEL_BITS;
        for (y, row) in frame.pixels.iter().enumerate() {
            for (x, color) in row.iter().enumerate() {
                back.set_pixel(
                    Point::new(x as i32, y as i32),
                    Color::new(
                        color.r.value() << shift,
                        color.g.value() << shift,
                        color.b.value() << shift,
                    ),
                );
            }
        }
        back.set_brightness(brightness.duty());
        let swap_start = Instant::now();
        let mut swap = hub75.swap(back).expect("only this task swaps buffers");
        swap.wait_for_done().await;
        back = swap.wait().expect("DMA keeps running across swaps");
        let swapped = Instant::now();
        DRAWN.fetch_add(1, Ordering::Relaxed);
        RENDER_MICROS.fetch_add(
            (swap_start - render_start).as_micros() as u32,
            Ordering::Relaxed,
        );
        SWAP_MICROS.fetch_add((swapped - swap_start).as_micros() as u32, Ordering::Relaxed);
    }
}

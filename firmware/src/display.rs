use core::sync::atomic::{AtomicU32, Ordering};

use album_art_core::protocol::{Brightness, Depth, PANEL_BITS, PanelColor, Report, SIZE, Target};
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::geometry::Point;
use esp_hal::peripherals::{DMA_I2S0, I2S0};
use esp_hal::time::Rate;
use esp_hub75::framebuffer::bitplane::plain::hold::{DmaFrameBuffer, Holds};
use esp_hub75::framebuffer::compute_rows;
use esp_hub75::{Color, Hub75, Hub75Config, Hub75Pins16};
use log::info;
use static_cell::ConstStaticCell;

const NROWS: usize = compute_rows(SIZE);
const PLANES: usize = PANEL_BITS as usize;
/// The ESP32's I2S peripheral tops out at 19 MHz.
const PIXEL_CLOCK: Rate = Rate::from_mhz(19);

type FrameBuffer = DmaFrameBuffer<NROWS, SIZE, PLANES>;

/// Dimming lengthens rows; below this the panel flickers on camera and in
/// peripheral vision, so dimmer settings shorten the planes instead.
const MIN_REFRESH_HZ: u32 = 180;
const MAX_ROW_CLOCKS: usize = (PIXEL_CLOCK.as_hz() / (MIN_REFRESH_HZ * NROWS as u32)) as usize;
const _: () = assert!(FrameBuffer::FULL_BRIGHTNESS_ROW_CLOCKS <= MAX_ROW_CLOCKS);

const STATS_INTERVAL: Duration = Duration::from_secs(5);

static SHOWN: AtomicU32 = AtomicU32::new(0);
static SWAP_MICROS: AtomicU32 = AtomicU32::new(0);

/// The panel as a destination for decoded frames: pixels go straight into
/// the back buffer, and [`Panel::present`] swaps it onto the panel.
pub struct Panel {
    hub75: Hub75<esp_hal::Async, FrameBuffer>,
    /// `None` only while a swap is in flight.
    back: Option<&'static mut FrameBuffer>,
    brightness: Brightness,
    /// The depth the latest brightness leaves, until the server is told.
    unreported: Option<Depth>,
}

impl Panel {
    /// Shows the frame written into the back buffer, once the DMA has moved
    /// over to it. Every frame writes every pixel, so the buffer coming back
    /// needs no erasing before the next one.
    pub async fn present(&mut self) {
        let back = self.back.take().expect("present is not re-entered");
        back.set_brightness(self.brightness.duty(), MAX_ROW_CLOCKS);
        let swap_start = Instant::now();
        let mut swap = self.hub75.swap(back).expect("only the panel swaps buffers");
        swap.wait_for_done().await;
        self.back = Some(swap.wait().expect("DMA keeps running across swaps"));
        SHOWN.fetch_add(1, Ordering::Relaxed);
        SWAP_MICROS.fetch_add(
            (Instant::now() - swap_start).as_micros() as u32,
            Ordering::Relaxed,
        );
    }

    /// What the server should hear since the last call.
    pub fn take_report(&mut self) -> Option<Report> {
        self.unreported.take().map(Report::Depth)
    }
}

impl Target for Panel {
    fn pixel(&mut self, x: usize, y: usize, color: PanelColor) {
        self.back
            .as_mut()
            .expect("pixels only arrive between swaps")
            .set_pixel(
                Point::new(x as i32, y as i32),
                Color::new(color.r.value(), color.g.value(), color.b.value()),
            );
    }

    /// Takes effect with the next frame; the server re-sends the current one
    /// after a brightness change.
    fn brightness(&mut self, brightness: Brightness) {
        self.brightness = brightness;
        let back = self.back.as_mut().expect("brightness only arrives between swaps");
        back.set_brightness(brightness.duty(), MAX_ROW_CLOCKS);
        let bits = back.lit_bits();
        self.unreported =
            Some(Depth::new(bits as u8).expect("the framebuffer has PANEL_BITS planes"));
        info!(
            "Display: brightness {}%, {bits} bits, refreshing at {} Hz",
            brightness.percent(),
            PIXEL_CLOCK.as_hz() as usize / (back.row_clocks() * NROWS),
        );
    }
}

pub fn start(
    spawner: Spawner,
    i2s: I2S0<'static>,
    dma: DMA_I2S0<'static>,
    pins: Hub75Pins16<'static>,
) -> Panel {
    // Each buffer is larger than the main stack, so constructing one at
    // runtime would overflow it; zeroed statics formatted in place also keep
    // a formatted copy of both out of the firmware image
    static FRONT: ConstStaticCell<FrameBuffer> = ConstStaticCell::new(FrameBuffer::unformatted());
    static BACK: ConstStaticCell<FrameBuffer> = ConstStaticCell::new(FrameBuffer::unformatted());
    static HOLDS: ConstStaticCell<Holds<NROWS>> = ConstStaticCell::new(Holds::unformatted());
    let holds = HOLDS.take();
    holds.format();
    let holds: &'static Holds<NROWS> = holds;
    let front = FRONT.take();
    let back = BACK.take();
    front.format(holds);
    back.format(holds);

    let hub75 = Hub75::new_async(
        i2s,
        pins,
        dma,
        esp_hub75::hub75_dma_descriptors!(FrameBuffer),
        Hub75Config::new().with_frequency(PIXEL_CLOCK),
        &*front,
    )
    .expect("HUB75 driver starts");

    spawner.spawn(stats_task().expect("stats task is spawned once"));
    Panel {
        hub75,
        back: Some(back),
        // The server sends brightness first; until then nothing is lit
        brightness: Brightness::OFF,
        unreported: None,
    }
}

/// Logs throughput while frames are arriving, and stays quiet otherwise.
#[embassy_executor::task]
async fn stats_task() {
    loop {
        Timer::after(STATS_INTERVAL).await;
        let shown = SHOWN.swap(0, Ordering::Relaxed);
        let swap = SWAP_MICROS.swap(0, Ordering::Relaxed);
        if shown == 0 {
            continue;
        }
        info!(
            "Display: {shown} frames shown in {}s; {}us waiting for the swap per frame",
            STATS_INTERVAL.as_secs(),
            swap / shown,
        );
    }
}

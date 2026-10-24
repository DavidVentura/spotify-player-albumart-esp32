use album_art_core::color::ColorCorrection;
use album_art_core::control::{Command, Tuning};
use album_art_core::thumbnail::{SIZE, Thumbnail};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embedded_graphics::geometry::Point;
use esp_hal::peripherals::{DMA_I2S0, I2S0};
use esp_hal::time::Rate;
use esp_hub75::framebuffer::bitplane::plain::DmaFrameBuffer;
use esp_hub75::framebuffer::compute_rows;
use esp_hub75::{Color, Hub75, Hub75Config, Hub75Pins16};
use log::info;
use static_cell::ConstStaticCell;

const NROWS: usize = compute_rows(SIZE);
const PLANES: usize = 6;
/// The ESP32's I2S peripheral tops out at 19 MHz.
const PIXEL_CLOCK: Rate = Rate::from_mhz(19);

type FrameBuffer = DmaFrameBuffer<NROWS, SIZE, PLANES>;

const REFRESH_HZ: u32 = esp_hub75::refresh_hz::<FrameBuffer>(PIXEL_CLOCK);
// Below this, the panel visibly flickers in peripheral vision
const _: () = assert!(REFRESH_HZ >= 120);

pub enum Screen {
    Blank,
    Cover(Thumbnail),
}

static SCREEN: Signal<CriticalSectionRawMutex, Screen> = Signal::new();
/// A queue rather than a signal, so commands sent back to back are all applied.
static COMMANDS: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();

pub fn show(screen: Screen) {
    SCREEN.signal(screen);
}

pub async fn adjust(command: Command) {
    COMMANDS.send(command).await;
}

pub fn start(
    spawner: Spawner,
    i2s: I2S0<'static>,
    dma: DMA_I2S0<'static>,
    pins: Hub75Pins16<'static>,
) {
    // Built at compile time: each buffer is larger than the main stack, so
    // constructing them at runtime would overflow it
    static FRONT: ConstStaticCell<FrameBuffer> = ConstStaticCell::new(FrameBuffer::new());
    static BACK: ConstStaticCell<FrameBuffer> = ConstStaticCell::new(FrameBuffer::new());
    let front = FRONT.take();
    let back = BACK.take();
    front.set_brightness(Tuning::DEFAULT.brightness.duty());

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
}

#[embassy_executor::task]
async fn display_task(
    hub75: Hub75<esp_hal::Async, FrameBuffer>,
    mut back: &'static mut FrameBuffer,
) {
    let mut screen = Screen::Blank;
    let mut tuning = Tuning::DEFAULT;
    let mut correction = ColorCorrection::new(tuning.gamma, tuning.white_balance);
    loop {
        match select(SCREEN.wait(), COMMANDS.receive()).await {
            Either::First(next) => screen = next,
            Either::Second(command) => {
                tuning = tuning.with(command);
                correction = ColorCorrection::new(tuning.gamma, tuning.white_balance);
                let (red, green, blue) = tuning.white_balance.percentages();
                info!(
                    "Display: brightness {}%, gamma {}, white balance {red}/{green}/{blue}",
                    tuning.brightness.percent(),
                    tuning.gamma.value(),
                );
            }
        }
        back.erase();
        if let Screen::Cover(thumbnail) = &screen {
            for (y, row) in thumbnail.pixels.iter().enumerate() {
                for (x, &pixel) in row.iter().enumerate() {
                    let pixel = correction.apply(pixel);
                    back.set_pixel(
                        Point::new(x as i32, y as i32),
                        Color::new(pixel.r, pixel.g, pixel.b),
                    );
                }
            }
        }
        back.set_brightness(tuning.brightness.duty());
        let mut swap = hub75.swap(back).expect("only this task swaps buffers");
        swap.wait_for_done().await;
        back = swap.wait().expect("DMA keeps running across swaps");
    }
}

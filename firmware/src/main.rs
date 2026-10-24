#![no_std]
#![no_main]

extern crate alloc;

mod config;
mod display;
mod link;
mod wifi;

use core::net::SocketAddrV4;

use embassy_executor::Spawner;
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Pin;
use esp_hal::ram;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::timer::timg::TimerGroup;
use esp_hub75::Hub75Pins16;
use esp_metadata_generated::memory_range;

esp_bootloader_esp_idf::esp_app_desc!();

const RECLAIMED_RAM: usize =
    memory_range!("DRAM2_UNINIT").end - memory_range!("DRAM2_UNINIT").start;
/// WiFi plus a few 12 KiB frames in flight; the rest of DRAM is main stack.
const MAIN_HEAP: usize = 48 * 1024;

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    heap_allocator!(#[ram(reclaimed)] size: RECLAIMED_RAM);
    heap_allocator!(size: MAIN_HEAP);

    let server: SocketAddrV4 = config::SERVER_ADDR
        .parse()
        .expect("SERVER_ADDR is an ipv4:port address");

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // On the ESP32 the entropy source samples the ADC through I2S0 and resets
    // it when dropped, which would stop the panel output; take the seed before
    // the panel driver claims I2S0
    let seed = {
        let _source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
        let trng = Trng::try_new().expect("TRNG source is running");
        (u64::from(trng.random()) << 32) | u64::from(trng.random())
    };

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

    let stack = wifi::connect(spawner, peripherals.WIFI, seed).await;
    spawner.spawn(link::run(stack, server).expect("link task is spawned once"));
    spawner.spawn(health().expect("health task is spawned once"));
}

#[embassy_executor::task]
async fn health() {
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
        log::info!(
            "Health: heap {} used, {} free; link received {} bytes",
            esp_alloc::HEAP.used(),
            esp_alloc::HEAP.free(),
            link::RECEIVED_BYTES.swap(0, core::sync::atomic::Ordering::Relaxed),
        );
    }
}

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, Interface, WifiController,
};
use log::{error, info};
use static_cell::StaticCell;

use crate::config::{WIFI_PASSWORD, WIFI_SSID};

/// DHCP and the server connection.
const SOCKETS: usize = 2;
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// In units of 0.25 dBm; ESP-IDF's default of 20 dBm.
const MAX_TX_POWER: i8 = 80;

/// Starts the station and returns once DHCP has configured the interface.
pub async fn connect(spawner: Spawner, wifi: WIFI<'static>, seed: u64) -> Stack<'static> {
    let station = Config::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID.try_into().expect("WIFI_SSID is a valid SSID"))
            .with_authentication(AuthenticationMethodConfig::WpaWpa2Personal(
                WIFI_PASSWORD
                    .try_into()
                    .expect("WIFI_PASSWORD is a valid WPA passphrase"),
            )),
    );
    let mut controller = WifiController::new(
        wifi,
        ControllerConfig::default().with_initial_config(station),
    )
    .expect("WiFi controller starts");
    // esp-radio defaults to 5 dBm, at which many access points never hear the
    // 4-way handshake
    controller
        .set_max_tx_power(MAX_TX_POWER)
        .expect("TX power is within range");

    static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(connection_task(controller).expect("connection task is spawned once"));
    spawner.spawn(net_task(runner).expect("net task is spawned once"));

    stack.wait_config_up().await;
    let address = stack.config_v4().expect("config is up").address;
    info!("WiFi: got {address}");
    stack
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(connected) => {
                info!("WiFi: connected {connected:?}");
                let disconnected = controller.wait_for_disconnect_async().await;
                error!("WiFi: disconnected {disconnected:?}");
            }
            Err(e) => error!("WiFi: failed to connect: {e:?}"),
        }
        Timer::after(RECONNECT_DELAY).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

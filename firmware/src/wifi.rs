use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, TimeoutError, Timer, with_timeout};
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
const SIGNAL_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// Replies to a weak station get lost now and then, and smoltcp's defaults
/// (10s, 5s) make each loss a long stall. It must stay above the server's
/// ping check (ISC dhcpd waits 1s before offering), or every retry would
/// discard the offer to the previous one.
const DHCP_RETRY: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(2);
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

    let mut dhcp = embassy_net::DhcpConfig::default();
    dhcp.retry_config.discover_timeout = DHCP_RETRY;
    dhcp.retry_config.initial_request_timeout = DHCP_RETRY;
    static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        embassy_net::Config::dhcpv4(dhcp),
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
                let disconnected = loop {
                    match with_timeout(SIGNAL_LOG_INTERVAL, controller.wait_for_disconnect_async())
                        .await
                    {
                        Ok(disconnected) => break disconnected,
                        Err(TimeoutError) => match controller.rssi() {
                            Ok(rssi) => info!("WiFi: access point heard at {rssi} dBm"),
                            Err(e) => error!("WiFi: reading signal strength: {e:?}"),
                        },
                    }
                };
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

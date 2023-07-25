#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use embassy_executor::_export::StaticCell;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Ipv4Address, Stack, StackResources, SmolDuration};
use examples_util::hal;

use embassy_executor::Executor;
use embassy_futures::join::join;
use embassy_time::{Duration, Timer, with_timeout};
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};
use esp_backtrace as _;
use esp_println::logger::init_logger;
use esp_println::println;
use esp_wifi::wifi::{WifiController, WifiDevice, WifiEvent, WifiMode, WifiState};
use esp_wifi::{initialize, EspWifiInitFor};
use hal::clock::{ClockControl, CpuClock};
use hal::Rng;
use hal::{embassy, peripherals::Peripherals, prelude::*, timer::TimerGroup, Rtc};

#[cfg(any(feature = "esp32c3", feature = "esp32c2", feature = "esp32c6"))]
use hal::system::SystemExt;

use log::{info, error};

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        let (x,) = STATIC_CELL.init(($val,));
        x
    }};
}

static EXECUTOR: StaticCell<Executor> = StaticCell::new();

#[entry]
fn main() -> ! {
    init_logger(log::LevelFilter::Info);

    let peripherals = Peripherals::take();

    let system = examples_util::system!(peripherals);
    let mut peripheral_clock_control = system.peripheral_clock_control;
    let clocks = examples_util::clocks!(system);
    examples_util::rtc!(peripherals);

    let timer = examples_util::timer!(peripherals, clocks, peripheral_clock_control);
    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    let wifi = examples_util::get_wifi!(peripherals);
    let (wifi_interface, controller) = esp_wifi::wifi::new_with_mode(&init, wifi, WifiMode::Sta);

    let timer_group0 = TimerGroup::new(peripherals.TIMG0, &clocks, &mut peripheral_clock_control);
    embassy::init(&clocks, timer_group0.timer0);

    let config = Config::Dhcp(Default::default());

    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = &*singleton!(Stack::new(
        wifi_interface,
        config,
        singleton!(StackResources::<3>::new()),
        seed
    ));

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(connection(controller)).ok();
        spawner.spawn(net_task(&stack)).ok();
        spawner.spawn(task(&stack)).ok();
    })
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.into(),
                password: PASSWORD.into(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static>>) {
    stack.run().await
}

#[embassy_executor::task]
async fn task(stack: &'static Stack<WifiDevice<'static>>) {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        let down = test_download(stack).await;
        let up = test_upload(stack).await;
        let updown = test_upload_download(stack).await;

        Timer::after(Duration::from_millis(10000)).await;
    }
}

const TEST_DURATION: usize = 15;
// const TEST_EXPECTED_DOWNLOAD_KBPS: usize = 400;
// const TEST_EXPECTED_UPLOAD_KBPS: usize = 400;
const RX_BUFFER_SIZE: usize = 8192;
const TX_BUFFER_SIZE: usize = 8192;
//const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(192, 168, 2, 221);
//const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(192, 168, 1, 20);
// const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(10, 42, 0, 1);
const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(192, 168, 0, 24);
const DOWNLOAD_PORT: u16 = 4321;
const UPLOAD_PORT: u16 = 4322;
const UPLOAD_DOWNLOAD_PORT: u16 = 4323;

async fn test_download(stack: &'static Stack<WifiDevice<'static>>) -> usize {
    info!("Testing download...");

    let mut rx_buffer = [0; RX_BUFFER_SIZE];
    let mut tx_buffer = [0; TX_BUFFER_SIZE];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(SmolDuration::from_secs(10)));

    let mut dummy_data = [0; 4096];

    info!("connecting to {:?}:{}...", SERVER_ADDRESS, DOWNLOAD_PORT);
    if let Err(e) = socket.connect((SERVER_ADDRESS, DOWNLOAD_PORT)).await {
        error!("connect error: {:?}", e);
        return 0;
    }
    info!("connected, testing...");

    let mut buf = [0; 4096];
    let mut total: usize = 0;
    with_timeout(Duration::from_secs(TEST_DURATION as _), async {
        loop {
            match socket.read(&mut dummy_data).await {
                Ok(0) => {
                    error!("read EOF");
                    return 0;
                }
                Ok(n) => total += n,
                Err(e) => {
                    error!("read error: {:?}", e);
                    return 0;
                }
            }
        }
    })
    .await
    .ok();

    let kbps = (total + 512) / 1024 / TEST_DURATION;
    info!("download: {} kB/s", kbps);
    kbps
}

async fn test_upload(stack: &'static Stack<WifiDevice<'static>>) -> usize {
    info!("Testing upload...");

    let mut rx_buffer = [0; RX_BUFFER_SIZE];
    let mut tx_buffer = [0; TX_BUFFER_SIZE];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(SmolDuration::from_secs(10)));

    info!("connecting to {:?}:{}...", SERVER_ADDRESS, UPLOAD_PORT);
    if let Err(e) = socket.connect((SERVER_ADDRESS, UPLOAD_PORT)).await {
        error!("connect error: {:?}", e);
        return 0;
    }
    info!("connected, testing...");

    let mut buf = [0; 4096];
    let mut total: usize = 0;
    with_timeout(Duration::from_secs(TEST_DURATION as _), async {
        loop {
            match socket.write(&buf).await {
                Ok(0) => {
                    error!("write zero?!??!?!");
                    return 0;
                }
                Ok(n) => total += n,
                Err(e) => {
                    error!("write error: {:?}", e);
                    return 0;
                }
            }
        }
    })
    .await
    .ok();

    let kbps = (total + 512) / 1024 / TEST_DURATION;
    info!("upload: {} kB/s", kbps);
    kbps
}

async fn test_upload_download(stack: &'static Stack<WifiDevice<'static>>) -> usize {
    info!("Testing upload+download...");

    let mut rx_buffer = [0; RX_BUFFER_SIZE];
    let mut tx_buffer = [0; TX_BUFFER_SIZE];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(SmolDuration::from_secs(10)));

    info!("connecting to {:?}:{}...", SERVER_ADDRESS, UPLOAD_DOWNLOAD_PORT);
    if let Err(e) = socket.connect((SERVER_ADDRESS, UPLOAD_DOWNLOAD_PORT)).await {
        error!("connect error: {:?}", e);
        return 0;
    }
    info!("connected, testing...");

    let (mut reader, mut writer) = socket.split();

    let mut tx_buf = [0; 4096];
    let mut rx_buf = [0; 4096];
    let mut total: usize = 0;
    let tx_fut = async {
        loop {
            match writer.write(&tx_buf).await {
                Ok(0) => {
                    error!("write zero?!??!?!");
                    return 0;
                }
                Ok(_) => {}
                Err(e) => {
                    error!("write error: {:?}", e);
                    return 0;
                }
            }
        }
    };

    let rx_fut = async {
        loop {
            match reader.read(&mut rx_buf).await {
                Ok(0) => {
                    error!("read EOF");
                    return 0;
                }
                Ok(n) => total += n,
                Err(e) => {
                    error!("read error: {:?}", e);
                    return 0;
                }
            }
        }
    };

    with_timeout(Duration::from_secs(TEST_DURATION as _), join(tx_fut, rx_fut))
        .await
        .ok();

    let kbps = (total + 512) / 1024 / TEST_DURATION;
    info!("upload+download: {} kB/s", kbps);
    kbps
}

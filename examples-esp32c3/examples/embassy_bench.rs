#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use embassy_executor::_export::StaticCell;
use embassy_futures::join::join;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Ipv4Address, Stack, StackResources};
#[path = "../../examples-util/util.rs"]
mod examples_util;
use examples_util::hal;

use embassy_executor::Executor;
use embassy_time::{Duration, Timer, with_timeout};
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};
use esp_backtrace as _;

use esp_println::println;
use esp_wifi::wifi::{WifiController, WifiDevice, WifiEvent, WifiMode, WifiState};
use esp_wifi::{initialize, EspWifiInitFor};
use hal::clock::ClockControl;
use hal::{embassy, peripherals::Peripherals, prelude::*, timer::TimerGroup};
use hal::{systimer::SystemTimer, Rng};

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
    #[cfg(feature = "log")]
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = Peripherals::take();

    let mut system = peripherals.SYSTEM.split();
    let clocks = ClockControl::max(system.clock_control).freeze();

    let timer = SystemTimer::new(peripherals.SYSTIMER).alarm0;
    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    let (wifi, ..) = peripherals.RADIO.split();
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiMode::Sta).unwrap();

    let timer_group0 = TimerGroup::new(
        peripherals.TIMG0,
        &clocks,
        &mut system.peripheral_clock_control,
    );
    embassy::init(&clocks, timer_group0.timer0);

    let config = Config::dhcpv4(Default::default());

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
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut rx_buffer = [0; RX_BUFFER_SIZE];
    let mut tx_buffer = [0; TX_BUFFER_SIZE];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

    loop {
        let down = test_download(stack, &mut socket).await;
        // let up = test_upload(stack).await;
        // let updown = test_upload_download(stack).await;

        Timer::after(Duration::from_millis(10000)).await;
    }
}

const TEST_DURATION: usize = 15;
const RX_BUFFER_SIZE: usize = 4096;
const TX_BUFFER_SIZE: usize = 4096;
//const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(192, 168, 2, 221);
//const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(192, 168, 1, 20);
// const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(10, 42, 0, 1);
const SERVER_ADDRESS: Ipv4Address = Ipv4Address::new(192, 168, 0, 24); // TODO via env
const DOWNLOAD_PORT: u16 = 4321;
const UPLOAD_PORT: u16 = 4322;
const UPLOAD_DOWNLOAD_PORT: u16 = 4323;

async fn test_download(stack: &'static Stack<WifiDevice<'static>>, socket: &mut TcpSocket<'_>) -> usize {
    println!("Testing download...");

    socket.abort();
    socket.set_timeout(Some(Duration::from_secs(10)));

    println!("connecting to {:?}:{}...", SERVER_ADDRESS, DOWNLOAD_PORT);
    if let Err(e) = socket.connect((SERVER_ADDRESS, DOWNLOAD_PORT)).await {
        println!("connect error: {:?}", e);
        return 0;
    }
    println!("connected, testing...");

    let mut buf = [0; RX_BUFFER_SIZE];
    let mut total: usize = 0;
    with_timeout(Duration::from_secs(TEST_DURATION as _), async {
        loop {
            match socket.read(&mut buf).await {
                Ok(0) => {
                    println!("read EOF");
                    return 0;
                }
                Ok(n) => total += n,
                Err(e) => {
                    println!("read error: {:?}", e);
                    return 0;
                }
            }
        }
    })
    .await
    .ok();

    let kbps = (total + 512) / 1024 / TEST_DURATION;
    println!("download: {} kB/s", kbps);
    kbps
}

// async fn test_upload(stack: &'static Stack<WifiDevice<'static>>) -> usize {
//     println!("Testing upload...");

//     let mut rx_buffer = [0; RX_BUFFER_SIZE];
//     let mut tx_buffer = [0; TX_BUFFER_SIZE];
//     let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
//     socket.set_timeout(Some(Duration::from_secs(10)));

//     println!("connecting to {:?}:{}...", SERVER_ADDRESS, UPLOAD_PORT);
//     if let Err(e) = socket.connect((SERVER_ADDRESS, UPLOAD_PORT)).await {
//         println!("connect error: {:?}", e);
//         return 0;
//     }
//     println!("connected, testing...");

//     let mut buf = [0; RX_BUFFER_SIZE];
//     let mut total: usize = 0;
//     with_timeout(Duration::from_secs(TEST_DURATION as _), async {
//         loop {
//             match socket.write(&buf).await {
//                 Ok(0) => {
//                     println!("write zero?!??!?!");
//                     return 0;
//                 }
//                 Ok(n) => total += n,
//                 Err(e) => {
//                     println!("write error: {:?}", e);
//                     return 0;
//                 }
//             }
//         }
//     })
//     .await
//     .ok();

//     let kbps = (total + 512) / 1024 / TEST_DURATION;
//     println!("upload: {} kB/s", kbps);
//     kbps
// }

// async fn test_upload_download(stack: &'static Stack<WifiDevice<'static>>) -> usize {
//     println!("Testing upload+download...");

//     let mut rx_buffer = [0; RX_BUFFER_SIZE];
//     let mut tx_buffer = [0; TX_BUFFER_SIZE];
//     let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
//     socket.set_timeout(Some(Duration::from_secs(10)));

//     println!("connecting to {:?}:{}...", SERVER_ADDRESS, UPLOAD_DOWNLOAD_PORT);
//     if let Err(e) = socket.connect((SERVER_ADDRESS, UPLOAD_DOWNLOAD_PORT)).await {
//         println!("connect error: {:?}", e);
//         return 0;
//     }
//     println!("connected, testing...");

//     let (mut reader, mut writer) = socket.split();

//     let mut tx_buf = [0; TX_BUFFER_SIZE];
//     let mut rx_buf = [0; RX_BUFFER_SIZE];
//     let mut total: usize = 0;
//     let tx_fut = async {
//         loop {
//             match writer.write(&tx_buf).await {
//                 Ok(0) => {
//                     println!("write zero?!??!?!");
//                     return 0;
//                 }
//                 Ok(_) => {}
//                 Err(e) => {
//                     println!("write error: {:?}", e);
//                     return 0;
//                 }
//             }
//         }
//     };

//     let rx_fut = async {
//         loop {
//             match reader.read(&mut rx_buf).await {
//                 Ok(0) => {
//                     println!("read EOF");
//                     return 0;
//                 }
//                 Ok(n) => total += n,
//                 Err(e) => {
//                     println!("read error: {:?}", e);
//                     return 0;
//                 }
//             }
//         }
//     };

//     with_timeout(Duration::from_secs(TEST_DURATION as _), join(tx_fut, rx_fut))
//         .await
//         .ok();

//     let kbps = (total + 512) / 1024 / TEST_DURATION;
//     println!("upload+download: {} kB/s", kbps);
//     kbps
// }

#![no_std]
#![no_main]

use examples_util::hal;

use embedded_io::blocking::*;
use embedded_svc::ipv4::Interface;
use embedded_svc::wifi::{AccessPointInfo, ClientConfiguration, Configuration, Wifi};

use esp_backtrace as _;
use esp_println::logger::init_logger;
use esp_println::{print, println};
use esp_wifi::wifi::utils::create_network_interface;
use esp_wifi::wifi::{WifiError, WifiMode};
use esp_wifi::wifi_interface::WifiStack;
use esp_wifi::{current_millis, initialize, EspWifiInitFor};
use hal::clock::{ClockControl, CpuClock};
use hal::Rng;
use hal::{peripherals::Peripherals, prelude::*, Rtc};
use smoltcp::iface::SocketStorage;
use smoltcp::wire::IpAddress;
use smoltcp::wire::Ipv4Address;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

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
    let mut socket_set_entries: [SocketStorage; 3] = Default::default();
    let (iface, device, mut controller, sockets) =
        create_network_interface(&init, wifi, WifiMode::Sta, &mut socket_set_entries);
    let wifi_stack = WifiStack::new(iface, device, sockets, current_millis);

    let client_config = Configuration::Client(ClientConfiguration {
        ssid: SSID.into(),
        password: PASSWORD.into(),
        ..Default::default()
    });
    let res = controller.set_configuration(&client_config);
    println!("wifi_set_configuration returned {:?}", res);

    controller.start().unwrap();
    println!("is wifi started: {:?}", controller.is_started());

    println!("Start Wifi Scan");
    let res: Result<(heapless::Vec<AccessPointInfo, 10>, usize), WifiError> = controller.scan_n();
    if let Ok((res, _count)) = res {
        for ap in res {
            println!("{:?}", ap);
        }
    }

    println!("{:?}", controller.get_capabilities());
    println!("wifi_connect {:?}", controller.connect());

    // wait to get connected
    println!("Wait to get connected");
    loop {
        let res = controller.is_connected();
        match res {
            Ok(connected) => {
                if connected {
                    break;
                }
            }
            Err(err) => {
                println!("{:?}", err);
                loop {}
            }
        }
    }
    println!("{:?}", controller.is_connected());

    // wait for getting an ip address
    println!("Wait to get an ip address");
    loop {
        wifi_stack.work();

        if wifi_stack.is_iface_up() {
            println!("got ip {:?}", wifi_stack.get_ip_info());
            break;
        }
    }

    println!("Start busy loop on main");

    let mut rx_buffer = [0u8; RX_BUFFER_SIZE];
    let mut tx_buffer = [0u8; TX_BUFFER_SIZE];
    let mut socket = wifi_stack.get_socket(&mut rx_buffer, &mut tx_buffer);

    test_download(&wifi_stack, &mut socket);

    loop {

    }
}

fn test_download<'a>(wifi_stack: &WifiStack<'a>, socket: &mut esp_wifi::wifi_interface::Socket<'a, 'a>) {

    println!("Making HTTP request");
    socket.work();

    socket
        .open(IpAddress::Ipv4(SERVER_ADDRESS), DOWNLOAD_PORT)
        .unwrap();

    
    let mut dummy_data = [0; 4096];

    let mut total = 0;
    let wait_end = current_millis() + (TEST_DURATION as u64 * 1000);
    loop {
        socket.work();
        if let Ok(len) = socket.read(&mut dummy_data) {
            total += len;
        } else {
            break;
        }

        if current_millis() > wait_end {
            println!("Finished!");
            break;
        }
    }
    println!();

    let kbps = (total + 512) / 1024 / TEST_DURATION;
    println!("download: {} kB/s", kbps);

    socket.disconnect();
}

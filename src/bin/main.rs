#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, ram, rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{WifiController, WifiDevice};
use tinymqtt::MqttClient;

use defmt::info;
use tmp1x2::{SlaveAddr, Tmp1x2};
use {esp_backtrace as _, esp_println as _};

esp_bootloader_esp_idf::esp_app_desc!();

// Channel to send temperature data from main loop to MQTT task
static TEMP_CHANNEL: Channel<CriticalSectionRawMutex, f32, 2> = Channel::new();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init(($val))
    }};
}

const SSID: &str = "Italia";
const PASSWORD: &str = "Jagvetinte10";
const MQTT_BROKER: (u8, u8, u8, u8) = (192, 168, 0, 108);

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let _sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0);

    let init = mk_static!(
        esp_radio::Controller<'static>,
        esp_radio::init().expect("Failed to initialize radio")
    );
    let (controller, interfaces) =
        esp_radio::wifi::new(init, peripherals.WIFI, esp_radio::wifi::Config::default()).unwrap();
    let wifi_device = interfaces.sta;

    println!("Wifi configured and started!");

    let config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_device,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );
    let stack = &*mk_static!(Stack<'static>, stack);

    spawner
        .spawn(connection(controller))
        .expect("connection spawn");
    spawner.spawn(net_task(runner)).expect("net task spawn");
    spawner.spawn(mqtt_task(stack)).expect("mqtt task spawn");

    // i2c config
    let i2cconfig = esp_hal::i2c::master::Config::default();
    let i2c = esp_hal::i2c::master::I2c::new(peripherals.I2C0, i2cconfig)
        .ok()
        .expect("error i2c conf")
        .with_sda(peripherals.GPIO47)
        .with_scl(peripherals.GPIO48);

    let addr = SlaveAddr::default();
    let mut sensor = Tmp1x2::new(i2c, addr);

    loop {
        if let Ok(temperature) = sensor.read_temperature() {
            info!("Temperature: {:?}ºC", temperature);
            // Send to MQTT task
            let _ = TEMP_CHANNEL.try_send(temperature);
        }

        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        if !controller.is_started().unwrap() {
            let client_config = esp_radio::wifi::ModeConfig::Client(
                esp_radio::wifi::ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            controller.start().unwrap();
        }

        match controller.connect() {
            Ok(_) => {
                println!("Wifi connected!");
                // Wait for disconnect
                loop {
                    Timer::after(Duration::from_secs(1)).await;
                    if !controller.is_connected().unwrap() {
                        break;
                    }
                }
                println!("Disconnected");
            }
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
            }
        }
        Timer::after(Duration::from_millis(5000)).await
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn mqtt_task(stack: &'static Stack<'static>) {
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];

    loop {
        println!("Waiting for network to be up...");
        loop {
            if stack.is_link_up() && stack.config_v4().is_some() {
                break;
            }
            Timer::after(Duration::from_millis(500)).await;
        }
        println!("Stack is up, connecting to MQTT broker...");

        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        let remote_endpoint = (
            embassy_net::IpAddress::v4(MQTT_BROKER.0, MQTT_BROKER.1, MQTT_BROKER.2, MQTT_BROKER.3),
            1883,
        );

        if let Err(e) = socket.connect(remote_endpoint).await {
            println!("MQTT Connect error: {:?}", e);
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        println!("Connected to MQTT broker!");

        let mut client: MqttClient<256> = MqttClient::new();
        if let Ok(packet) = client.connect("esp32s3-sensor", None) {
            let _ = socket.write_all(packet).await;
        }

        loop {
            // Wait for temperature from main loop
            let temp = TEMP_CHANNEL.receive().await;

            // Format temperature as string
            let mut payload_buf = [0u8; 32];
            let payload = format_temp(temp, &mut payload_buf);

            if let Ok(packet) = client.publish("esp32/temperature", payload.as_bytes()) {
                if let Err(_) = socket.write_all(packet).await {
                    println!("MQTT Publish failed, connection lost");
                    break;
                }
                println!("Published temperature: {} to esp32/temperature", payload);
            }
        }

        Timer::after(Duration::from_secs(5)).await;
    }
}

fn format_temp(temp: f32, buf: &mut [u8]) -> &str {
    let mut itoa_buf = itoa::Buffer::new();
    let integer = temp as i32;
    let fraction = ((temp - integer as f32).abs() * 100.0) as i32;

    let s_int = itoa_buf.format(integer);
    let mut cursor = 0;
    buf[cursor..cursor + s_int.len()].copy_from_slice(s_int.as_bytes());
    cursor += s_int.len();
    buf[cursor] = b'.';
    cursor += 1;

    let mut itoa_buf = itoa::Buffer::new();
    let s_frac = itoa_buf.format(fraction);
    buf[cursor..cursor + s_frac.len()].copy_from_slice(s_frac.as_bytes());
    cursor += s_frac.len();

    unsafe { core::str::from_utf8_unchecked(&buf[..cursor]) }
}

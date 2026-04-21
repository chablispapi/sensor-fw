#![no_std]
#![no_main]

use core::time::Duration as SleepDuration;

use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_time::{Duration as EmbassyDuration, Timer};
use embedded_io_async::Write;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, ram, rng::Rng,
    rtc_cntl::{sleep::TimerWakeupSource, Rtc},
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::wifi::{WifiController, WifiDevice, WifiError};

use defmt::info;
use tmp1x2::{SlaveAddr, Tmp1x2};
use {esp_backtrace as _, esp_println as _};

esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init(($val))
    }};
}

const SSID: &str = "Italia";
const PASSWORD: &str = "Jagvetinte10";
const MQTT_BROKER: (u8, u8, u8, u8) = (192, 168, 0, 108);
const MQTT_CLIENT_ID: &str = "esp32s3-sensor";
const MQTT_TOPIC: &str = "esp32/temperature";
const DEEP_SLEEP_INTERVAL: SleepDuration = SleepDuration::from_secs(10 * 60);
const RETRY_DELAY: EmbassyDuration = EmbassyDuration::from_secs(5);

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

    // i2c config
    let i2cconfig =
        esp_hal::i2c::master::Config::default().with_frequency(esp_hal::time::Rate::from_khz(400));
    let i2c = esp_hal::i2c::master::I2c::new(peripherals.I2C0, i2cconfig)
        .ok()
        .expect("error i2c conf")
        .with_sda(peripherals.GPIO47)
        .with_scl(peripherals.GPIO48);

    let addr = SlaveAddr::default();
    let mut sensor = Tmp1x2::new(i2c, addr);
    let mut rtc = Rtc::new(peripherals.LPWR);

    loop {
        let temperature = loop {
            match sensor.read_temperature() {
                Ok(temperature) => break temperature,
                Err(err) => {
                    println!("Temperature read failed: {:?}", err);
                    Timer::after(RETRY_DELAY).await;
                }
            }
        };

        info!("Temperature: {:?}ºC", temperature);

        loop {
            match publish_temperature_once(stack, temperature).await {
                Ok(()) => break,
                Err(err) => {
                    println!("Publish failed: {err}");
                    Timer::after(RETRY_DELAY).await;
                }
            }
        }

        println!("Entering deep sleep for 10 minutes...");
        Timer::after(EmbassyDuration::from_millis(100)).await;
        let timer = TimerWakeupSource::new(DEEP_SLEEP_INTERVAL);
        rtc.sleep_deep(&[&timer]);
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = esp_radio::wifi::ModeConfig::Client(
                esp_radio::wifi::ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            if let Err(e) = controller.set_config(&client_config) {
                println!("Failed to apply WiFi config: {e:?}");
                Timer::after(RETRY_DELAY).await;
                continue;
            }

            if let Err(e) = controller.start() {
                println!("Failed to start WiFi: {e:?}");
                Timer::after(RETRY_DELAY).await;
                continue;
            }
        }

        match controller.connect() {
            Ok(_) => {
                println!("Wifi connected!");
                // Wait for disconnect
                loop {
                    Timer::after(EmbassyDuration::from_secs(1)).await;
                    match controller.is_connected() {
                        Ok(true) => {}
                        Ok(false) | Err(WifiError::Disconnected) => break,
                        Err(e) => {
                            println!("WiFi status check failed: {e:?}");
                            break;
                        }
                    }
                }
                println!("Disconnected");
            }
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
            }
        }
        Timer::after(EmbassyDuration::from_millis(5000)).await
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

async fn publish_temperature_once(stack: &'static Stack<'static>, temp: f32) -> Result<(), &'static str> {
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];
    let mut mqtt_rx_buf = [0; 256];
    let mut mqtt_tx_buf = [0; 256];

    println!("Waiting for network to be up...");
    loop {
        if stack.is_link_up() && stack.config_v4().is_some() {
            break;
        }
        Timer::after(EmbassyDuration::from_millis(500)).await;
    }
    println!("Stack is up, connecting to MQTT broker...");

    let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
    let remote_endpoint = (
        embassy_net::IpAddress::v4(MQTT_BROKER.0, MQTT_BROKER.1, MQTT_BROKER.2, MQTT_BROKER.3),
        1883,
    );

    socket
        .connect(remote_endpoint)
        .await
        .map_err(|_| "MQTT TCP connect failed")?;

    println!("Connected to MQTT broker!");

    let connect_packet =
        build_mqtt_connect_packet(MQTT_CLIENT_ID, &mut mqtt_tx_buf).ok_or("MQTT CONNECT packet build failed")?;

    socket
        .write_all(connect_packet)
        .await
        .map_err(|_| "MQTT CONNECT write failed")?;

    let connack_len = match socket.read(&mut mqtt_rx_buf).await {
        Ok(0) => return Err("MQTT broker closed connection before CONNACK"),
        Ok(len) => len,
        Err(_) => return Err("MQTT CONNACK read failed"),
    };

    parse_mqtt_connack(&mqtt_rx_buf[..connack_len])?;

    let mut payload_buf = [0u8; 32];
    let payload = format_temp(temp, &mut payload_buf);
    let publish_packet = build_mqtt_publish_packet(MQTT_TOPIC, payload.as_bytes(), &mut mqtt_tx_buf)
        .ok_or("MQTT publish packet build failed")?;

    socket
        .write_all(publish_packet)
        .await
        .map_err(|_| "MQTT publish write failed")?;

    println!("Published temperature: {} to {}", payload, MQTT_TOPIC);
    Ok(())
}

fn build_mqtt_connect_packet<'a>(client_id: &str, buf: &'a mut [u8]) -> Option<&'a [u8]> {
    let protocol_name = b"MQTT";
    let remaining_len = 10usize.checked_add(2)?.checked_add(client_id.len())?;
    let fixed_header_len = mqtt_remaining_len_bytes(remaining_len)?;
    let total_len = 1usize.checked_add(fixed_header_len)?.checked_add(remaining_len)?;
    if total_len > buf.len() {
        return None;
    }

    let mut cursor = 0;
    buf[cursor] = 0x10;
    cursor += 1;
    cursor += write_remaining_len(remaining_len, &mut buf[cursor..])?;
    cursor += write_mqtt_string(protocol_name, &mut buf[cursor..])?;

    buf[cursor] = 0x04;
    cursor += 1;
    buf[cursor] = 0x02;
    cursor += 1;
    buf[cursor] = 0x00;
    cursor += 1;
    buf[cursor] = 0x00;
    cursor += 1;
    cursor += write_mqtt_string(client_id.as_bytes(), &mut buf[cursor..])?;

    Some(&buf[..cursor])
}

fn build_mqtt_publish_packet<'a>(
    topic: &str,
    payload: &[u8],
    buf: &'a mut [u8],
) -> Option<&'a [u8]> {
    let remaining_len = 2usize
        .checked_add(topic.len())?
        .checked_add(payload.len())?;
    let fixed_header_len = mqtt_remaining_len_bytes(remaining_len)?;
    let total_len = 1usize.checked_add(fixed_header_len)?.checked_add(remaining_len)?;
    if total_len > buf.len() {
        return None;
    }

    let mut cursor = 0;
    buf[cursor] = 0x30;
    cursor += 1;
    cursor += write_remaining_len(remaining_len, &mut buf[cursor..])?;
    cursor += write_mqtt_string(topic.as_bytes(), &mut buf[cursor..])?;
    buf[cursor..cursor + payload.len()].copy_from_slice(payload);
    cursor += payload.len();

    Some(&buf[..cursor])
}

fn parse_mqtt_connack(packet: &[u8]) -> Result<(), &'static str> {
    if packet.len() < 4 {
        return Err("short CONNACK");
    }
    if packet[0] != 0x20 {
        return Err("unexpected packet type");
    }
    if packet[1] != 0x02 {
        return Err("unexpected CONNACK length");
    }
    if packet[3] != 0x00 {
        return Err("connection refused");
    }
    Ok(())
}

fn mqtt_remaining_len_bytes(len: usize) -> Option<usize> {
    match len {
        0..=127 => Some(1),
        128..=16_383 => Some(2),
        16_384..=2_097_151 => Some(3),
        2_097_152..=268_435_455 => Some(4),
        _ => None,
    }
}

fn write_remaining_len(mut len: usize, buf: &mut [u8]) -> Option<usize> {
    let mut cursor = 0;
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        *buf.get_mut(cursor)? = byte;
        cursor += 1;
        if len == 0 {
            return Some(cursor);
        }
    }
}

fn write_mqtt_string(value: &[u8], buf: &mut [u8]) -> Option<usize> {
    let len = u16::try_from(value.len()).ok()? as usize;
    if len + 2 > buf.len() {
        return None;
    }
    buf[0] = ((len >> 8) & 0xff) as u8;
    buf[1] = (len & 0xff) as u8;
    buf[2..2 + len].copy_from_slice(value);
    Some(len + 2)
}

fn format_temp(temp: f32, buf: &mut [u8]) -> &str {
    let scaled_f = temp * 100.0;
    let scaled = if scaled_f >= 0.0 {
        (scaled_f + 0.5) as i32
    } else {
        (scaled_f - 0.5) as i32
    };
    let integer = scaled / 100;
    let fraction = (scaled % 100).abs();

    let mut itoa_buf = itoa::Buffer::new();
    let s_int = itoa_buf.format(integer);
    let mut cursor = 0;
    buf[cursor..cursor + s_int.len()].copy_from_slice(s_int.as_bytes());
    cursor += s_int.len();
    if integer == 0 && scaled < 0 {
        buf.copy_within(..cursor, 1);
        buf[0] = b'-';
        cursor += 1;
    }
    buf[cursor] = b'.';
    cursor += 1;

    let mut itoa_buf = itoa::Buffer::new();
    let s_frac = itoa_buf.format(fraction);
    if fraction < 10 {
        buf[cursor] = b'0';
        cursor += 1;
    }
    buf[cursor..cursor + s_frac.len()].copy_from_slice(s_frac.as_bytes());
    cursor += s_frac.len();

    unsafe { core::str::from_utf8_unchecked(&buf[..cursor]) }
}

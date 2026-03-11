#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use defmt::info;
use esp_hal::clock::CpuClock;
use esp_hal::i2c::master::{Config, I2c, Operation};
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::{main, time::Rate};
use esp_radio::wifi;
use {esp_backtrace as _, esp_println as _};

use tmp1x2::{SlaveAddr, Tmp1x2};

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[main]
fn main() -> ! {
    // generator version: 1.2.0

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);
    let radio_init = esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller");
    let (mut _wifi_controller, _interfaces) =
        esp_radio::wifi::new(&radio_init, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    //i2c conf
    let i2cconfig = esp_hal::i2c::master::Config::default();
    let mut i2c = esp_hal::i2c::master::I2c::new(peripherals.I2C0, i2cconfig)
        .ok()
        .expect("error i2c conf")
        .with_sda(peripherals.GPIO47)
        .with_scl(peripherals.GPIO48);

    /*
    const DEVICE_ADDR: u8 = 0x48;
    let mut read_buffer = [0u8; 22];
    i2c.read(DEVICE_ADDR, &mut read_buffer)
        .ok()
        .expect("error reading");
    */

    let addr = SlaveAddr::default();
    let mut sensor = Tmp1x2::new(i2c, addr);

    loop {
        let mut temperature = sensor.read_temperature().unwrap();
        info!("Temperature: {:?}ºC", temperature);

        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(500) {}
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0/examples
}

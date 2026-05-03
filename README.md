# sensor-fw

Embedded Rust firmware for an ESP32-S3-based temperature sensor node. Reads temperature from a TMP1x2 sensor over I2C and publishes it to an MQTT broker over WiFi, then enters deep sleep to conserve power.

## Hardware

- **MCU**: ESP32-S3
- **Sensor**: TMP112 / TMP102 (I2C address default, SDA on GPIO47, SCL on GPIO48)
- **Communication**: WiFi (802.11) → MQTT

## Behavior

1. Boot and connect to a configured WiFi network (DHCP)
2. Read temperature from the TMP1x2 sensor
3. Connect to the MQTT broker and publish the reading to `esp32/temperature`
4. Enter deep sleep for 10 minutes
5. Wake up and repeat

## Configuration

Network and MQTT settings are constants in `src/bin/main.rs`:

| Constant | Description |
|----------|-------------|
| `SSID` | WiFi network name |
| `PASSWORD` | WiFi password |
| `MQTT_BROKER` | Broker IP address (tuple) |
| `MQTT_CLIENT_ID` | MQTT client identifier |
| `MQTT_TOPIC` | Topic to publish to |
| `DEEP_SLEEP_INTERVAL` | Duration between readings |

## Building

Requires the [esp Rust toolchain](https://github.com/esp-rs/rust-build):

```bash
# Install espup and set up the toolchain
cargo install espup
espup install

# Build
cargo build --release
```

## Flashing

```bash
cargo install espflash
espflash flash --monitor target/xtensa-esp32s3-none-elf/release/sensor-fw
```

## Dependencies

Key crates:

- `esp-hal` / `esp-rtos` / `esp-radio` — Hardware abstraction and WiFi
- `embassy-executor` / `embassy-net` / `embassy-time` — Async runtime and networking
- `tmp1x2` — TMP112/TMP102 I2C driver
- `smoltcp` — TCP/IP stack
- `defmt` — Lightweight logging

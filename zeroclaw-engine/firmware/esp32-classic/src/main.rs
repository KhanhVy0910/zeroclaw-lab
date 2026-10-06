//! ZeroClaw ESP32 Classic firmware
//!
//! Current phase:
//! - UART0: USB flash/debug communication
//! - Wi-Fi SoftAP: laptop <-> ESP32
//! - GPIO2 and GPIO13: output control
//!
//! Runtime architecture:
//!
//! Laptop
//!   |
//!   | Wi-Fi
//!   v
//! ESP32 SoftAP
//!   |
//!   +-- GPIO
//!
//! HTTP/REST will be added in the next phase.

use core::convert::TryInto;

use embedded_svc::wifi::{
    self,
    AccessPointConfiguration,
    AuthMethod,
};
use embedded_svc::io::Write;
use std::sync::{Arc, Mutex};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::uart::{UartConfig, UartDriver};
use esp_idf_svc::hal::units::Hertz;

use esp_idf_svc::http::server::{
    Configuration as HttpServerConfiguration,
    EspHttpServer,
};

use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};

use heapless::{String, Vec};
use log::info;

use zeroclaw_fw_protocol::{copy_id, write_err, write_ok, Command};


// ============================================================
// Wi-Fi configuration
// ============================================================

const WIFI_SSID: &str = "Zeroclaw-ESP32";
const WIFI_PASSWORD: &str = "zeroclaw123";
const WIFI_CHANNEL: u8 = 6;
const HTTP_PORT: u16 = 80;

// ============================================================
// GPIO capabilities
// ============================================================

const CAPABILITIES_RESULT: &str =
    r#"{\"gpio_output\":[2,4,5,12,13,14,15,16,17,18,19,21,22,23,25,26,27,32,33],\"gpio_input\":[2,4,5,12,13,14,15,16,17,18,19,21,22,23,25,26,27,32,33,34,35,36,39]}"#;


// ============================================================
// Main
// ============================================================

fn main() -> anyhow::Result<()> {
    // Required by esp-idf-svc.
    esp_idf_svc::sys::link_patches();

    // Initialize ESP-IDF logger.
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("========================================");
    info!("ZeroClaw ESP32 Classic starting...");
    info!("========================================");

    let peripherals = Peripherals::take()?;
    let pins = peripherals.pins;

    // --------------------------------------------------------
    // GPIO
    // --------------------------------------------------------

    // GPIO2 and GPIO13 are currently used as outputs.
    let gpio2 = Arc::new(Mutex::new(
    PinDriver::output(pins.gpio2)?
));

let gpio13 = Arc::new(Mutex::new(
    PinDriver::output(pins.gpio13)?
));

    // --------------------------------------------------------
    // UART0
    // --------------------------------------------------------

    // UART0:
    // TX = GPIO1
    // RX = GPIO3
    //
    // USB is still used for:
    // - firmware flashing
    // - monitor/debug
    //
    // Runtime control will later use Wi-Fi.
    let uart_config = UartConfig::new()
        .baudrate(Hertz(115_200));

    let uart = UartDriver::new(
        peripherals.uart0,
        pins.gpio1,
        pins.gpio3,
        Option::<esp_idf_svc::hal::gpio::Gpio0>::None,
        Option::<esp_idf_svc::hal::gpio::Gpio0>::None,
        &uart_config,
    )?;

    info!("UART0 ready: 115200 baud");

    // --------------------------------------------------------
    // Wi-Fi SoftAP
    // --------------------------------------------------------

    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let wifi = EspWifi::new(
        peripherals.modem,
        sysloop.clone(),
        Some(nvs),
    )?;

    let mut wifi = BlockingWifi::wrap(
        wifi,
        sysloop,
    )?;

    start_wifi_ap(&mut wifi)?;

    let gpio2_http = Arc::clone(&gpio2);
    let gpio13_http = Arc::clone(&gpio13);

    let _http_server = start_http_server(gpio2_http, gpio13_http)?;

    // --------------------------------------------------------
    // Wi-Fi successfully started
    // --------------------------------------------------------

    info!("========================================");
    info!("Wi-Fi SoftAP started");
    info!("SSID: {}", WIFI_SSID);
    info!("Password: {}", WIFI_PASSWORD);
    info!("AP IP: 192.168.4.1");
    info!("========================================");

    info!("Waiting for UART commands...");

    // --------------------------------------------------------
    // UART command loop
    // --------------------------------------------------------

    let mut buf = [0u8; 512];

    let mut line: Vec<u8, 400> = Vec::new();

    let mut resp_buf: String<256> = String::new();

    loop {
        match uart.read(&mut buf, 100) {
            Ok(0) => continue,

            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' {
                        if !line.is_empty() {
                            let mut gpio2_guard = gpio2.lock().unwrap();
                            let mut gpio13_guard = gpio13.lock().unwrap();

                            handle_request(
                                &line,
                                &mut *gpio2_guard,
                                &mut *gpio13_guard,
                                 &mut resp_buf,
                            );

                            let _ = uart.write(resp_buf.as_bytes());
                            let _ = uart.write(b"\n");

                            line.clear();
                        }
                    } else if line.push(b).is_err() {
                        // Request too long.
                        line.clear();
                    }
                }
            }

            Err(_) => {}
        }
    }
}


// ============================================================
// Wi-Fi SoftAP
// ============================================================

fn start_wifi_ap(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
) -> anyhow::Result<()> {

    info!("Starting Wi-Fi SoftAP...");
    info!("SSID: {}", WIFI_SSID);

    let wifi_configuration =
        wifi::Configuration::AccessPoint(
            AccessPointConfiguration {
                ssid: WIFI_SSID.try_into().unwrap(),

                ssid_hidden: false,

                auth_method: AuthMethod::WPA2Personal,

                password: WIFI_PASSWORD.try_into().unwrap(),

                channel: WIFI_CHANNEL,

                ..Default::default()
            },
        );

    // Apply AP configuration.
    wifi.set_configuration(&wifi_configuration)?;

    info!("Wi-Fi configuration applied");

    // Start Wi-Fi.
    wifi.start()?;

    info!("Wi-Fi driver started");

    // Wait until the network interface is ready.
    wifi.wait_netif_up()?;

    info!("Wi-Fi network interface is up");

    Ok(())
}
// ============================================================
// HTTP server
// ============================================================
 
fn start_http_server<G2, G13>(
    gpio2: Arc<Mutex<PinDriver<'static, G2>>>,
    gpio13: Arc<Mutex<PinDriver<'static, G13>>>,
) -> anyhow::Result<EspHttpServer<'static>>
where
    G2: esp_idf_svc::hal::gpio::OutputMode + Send + 'static,
    G13: esp_idf_svc::hal::gpio::OutputMode + Send + 'static,
{
    info!("Starting HTTP server...");

    let server_config = HttpServerConfiguration {
        http_port: HTTP_PORT,
        ..Default::default()
    };

    let mut server = EspHttpServer::new(&server_config)?;

    // --------------------------------------------------------
    // GET /status
    // --------------------------------------------------------

    server.fn_handler(
        "/status",
        embedded_svc::http::Method::Get,
        |request| {
            let body = r#"{
  "status": "ok",
  "device": "esp32-classic"
}"#;

            let mut response = request.into_ok_response()?;
            response.write_all(body.as_bytes())?;

            Ok::<(), anyhow::Error>(())
        },
    )?;

    // --------------------------------------------------------
    // POST /gpio/write
    // --------------------------------------------------------

    let gpio2_http = Arc::clone(&gpio2);
    let gpio13_http = Arc::clone(&gpio13);

    server.fn_handler(
        "/gpio/write",
        embedded_svc::http::Method::Post,
        move |mut request| {
            let mut body = [0u8; 128];
            let mut total = 0usize;

            loop {
                if total >= body.len() {
                    anyhow::bail!("Request body too large");
                }

                let read_len = request.read(&mut body[total..])?;

                if read_len == 0 {
                    break;
                }

                total += read_len;
            }

            let body_str =
                core::str::from_utf8(&body[..total])
                    .map_err(|_| anyhow::anyhow!("Invalid UTF-8"))?;

            info!("GPIO HTTP body: {}", body_str);

            let pin = parse_json_i32(body_str, "pin")
                .ok_or_else(|| anyhow::anyhow!("Missing pin"))?;

            let value = parse_json_i32(body_str, "value")
                .ok_or_else(|| anyhow::anyhow!("Missing value"))?;

            if value != 0 && value != 1 {
                anyhow::bail!("value must be 0 or 1");
            }

            match pin {
                2 => {
                    let mut gpio = gpio2_http
                        .lock()
                        .map_err(|_| anyhow::anyhow!("GPIO2 lock failed"))?;

                    gpio.set_level(
                        esp_idf_svc::hal::gpio::Level::from(value != 0)
                    )?;
                }

                13 => {
                    let mut gpio = gpio13_http
                        .lock()
                        .map_err(|_| anyhow::anyhow!("GPIO13 lock failed"))?;

                    gpio.set_level(
                        esp_idf_svc::hal::gpio::Level::from(value != 0)
                    )?;
                }

                _ => {
                    let mut response = request.into_response(
                        400,
                        Some("Bad Request"),
                        &[("Content-Type", "application/json")],
                    )?;

                    response.write_all(
                        br#"{"status":"error","message":"Unsupported GPIO pin"}"#,
                    )?;

                    return Ok::<(), anyhow::Error>(());
                }
            }

            let mut response = request.into_ok_response()?;

            let mut result = String::<128>::new();

            let _ = core::fmt::Write::write_fmt(
                &mut result,
                format_args!(
                    r#"{{"status":"ok","pin":{},"value":{}}}"#,
                    pin,
                    value
                ),
            );

            response.write_all(result.as_bytes())?;

            Ok::<(), anyhow::Error>(())
        },
    )?;

    info!("HTTP server started");
    info!("GET /status registered");
    info!("POST /gpio/write registered");

    Ok(server)
}
//=========================================
// prase json 
fn parse_json_i32(body: &str, key: &str) -> Option<i32> {
    // Support both:
    // {"pin":2,"value":1}
    // {pin:2,value:1}
    //
    // Lightweight parser for the ESP32 HTTP API.

    let quoted_key = {
        let mut pattern = String::<32>::new();

        let _ = core::fmt::Write::write_fmt(
            &mut pattern,
            format_args!(r#""{}""#, key),
        );

        pattern
    };

    let (key_pos, key_len) =
        if let Some(pos) = body.find(quoted_key.as_str()) {
            (pos, quoted_key.len())
        } else if let Some(pos) = body.find(key) {
            (pos, key.len())
        } else {
            return None;
        };

    let after_key = &body[key_pos + key_len..];

    let colon_pos = after_key.find(':')?;

    let value_part =
        after_key[colon_pos + 1..].trim_start();

    let mut end = 0;

    for (i, ch) in value_part.char_indices() {
        if !(ch.is_ascii_digit() || (i == 0 && ch == '-')) {
            break;
        }

        end = i + ch.len_utf8();
    }

    if end == 0 {
        return None;
    }

    value_part[..end].parse::<i32>().ok()
}

// ============================================================
// UART command handling
// ============================================================

fn handle_request<G2, G13>(
    line: &[u8],

    gpio2: &mut PinDriver<'_, G2>,

    gpio13: &mut PinDriver<'_, G13>,

    resp_buf: &mut String<256>,
)
where
    G2: esp_idf_svc::hal::gpio::OutputMode,

    G13: esp_idf_svc::hal::gpio::OutputMode,
{
    let mut id_buf = [0u8; 32];

    let id_len = copy_id(line, &mut id_buf);

    let id_str =
        core::str::from_utf8(&id_buf[..id_len])
            .unwrap_or("0");

    match Command::from_line(line) {

        // ----------------------------------------------------
        // Capabilities
        // ----------------------------------------------------

        Some(Command::Capabilities) => {
            write_ok(
                resp_buf,
                id_str,
                CAPABILITIES_RESULT,
            );
        }

        // ----------------------------------------------------
        // GPIO read
        // ----------------------------------------------------

        Some(Command::GpioRead { pin }) => {
            match gpio_read(pin) {

                Ok(value) => {

                    let mut value_buf: String<8> =
                        String::new();

                    let _ =
                        core::fmt::Write::write_fmt(
                            &mut value_buf,
                            format_args!("{value}"),
                        );

                    write_ok(
                        resp_buf,
                        id_str,
                        &value_buf,
                    );
                }

                Err(e) => {
                    write_err(
                        resp_buf,
                        id_str,
                        &e.to_string(),
                    );
                }
            }
        }

        // ----------------------------------------------------
        // GPIO write
        // ----------------------------------------------------

        Some(Command::GpioWrite {
            pin,
            value,
        }) => {

            match gpio_write(
                gpio2,
                gpio13,
                pin,
                value,
            ) {

                Ok(()) => {
                    write_ok(
                        resp_buf,
                        id_str,
                        "done",
                    );
                }

                Err(e) => {
                    write_err(
                        resp_buf,
                        id_str,
                        &e.to_string(),
                    );
                }
            }
        }

        // ----------------------------------------------------
        // Ping
        // ----------------------------------------------------

        Some(Command::Ping) => {
            write_ok(
                resp_buf,
                id_str,
                "pong",
            );
        }

        // ----------------------------------------------------
        // Unknown command
        // ----------------------------------------------------

        None => {
            write_err(
                resp_buf,
                id_str,
                "Unknown command",
            );
        }
    }
}


// ============================================================
// GPIO read
// ============================================================

fn gpio_read(
    _pin: i32,
) -> anyhow::Result<u8> {

    // TODO:
    //
    // Implement actual GPIO input drivers later.
    //
    // Current firmware only keeps GPIO2 and GPIO13
    // as output drivers.

    Ok(0)
}


// ============================================================
// GPIO write
// ============================================================

fn gpio_write<G2, G13>(
    gpio2: &mut PinDriver<'_, G2>,

    gpio13: &mut PinDriver<'_, G13>,

    pin: i32,

    value: i32,
)
    -> anyhow::Result<()>
where
    G2: esp_idf_svc::hal::gpio::OutputMode,

    G13: esp_idf_svc::hal::gpio::OutputMode,
{
    let level =
        esp_idf_svc::hal::gpio::Level::from(
            value != 0
        );

    match pin {

        2 => {
            gpio2.set_level(level)?;
        }

        13 => {
            gpio13.set_level(level)?;
        }

        _ => {
            anyhow::bail!(
                "Pin {} not configured (add to gpio_write)",
                pin
            );
        }
    }

    Ok(())
}
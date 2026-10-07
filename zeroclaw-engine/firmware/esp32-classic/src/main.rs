//! ZeroClaw ESP32 Classic firmware
//!
//! Current phase:
//! - UART0: USB flash/debug communication (GPIO1 TX / GPIO3 RX)
//! - Wi-Fi SoftAP: laptop <-> ESP32
//! - HTTP server: GET /status, POST /gpio/write, GET /gpio/read?pin=X, GET /gpio/read
//! - GpioManager: generic GPIO output & input control
//!
//! Runtime architecture:
//!
//! Laptop
//!    |
//!    | Wi-Fi
//!    v
//! ESP32 SoftAP
//!    |
//!    v
//! HTTP Server --> GpioManager --> GPIO outputs & inputs

use core::convert::TryInto;
use core::fmt;

use embedded_svc::io::Write;
use embedded_svc::wifi::{self, AccessPointConfiguration, AuthMethod};
use std::sync::{Arc, Mutex};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{Input, InputOutput, Level, Output, PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::uart::{UartConfig, UartDriver};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys::EspError;

use esp_idf_svc::http::server::{
    Configuration as HttpServerConfiguration, EspHttpServer,
};

use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};

use heapless::{String, Vec};
use log::info;

use zeroclaw_fw_protocol::{copy_id, write_err, write_ok, Command};

// ============================================================
// Wi-Fi / HTTP configuration
// ============================================================

const WIFI_SSID: &str = "Zeroclaw-ESP32";
const WIFI_PASSWORD: &str = "zeroclaw123";
const WIFI_CHANNEL: u8 = 6;
const HTTP_PORT: u16 = 80;

// ============================================================
// GPIO capabilities
// ============================================================

const CAPABILITIES_RESULT: &str =
    r#"{"gpio_output":[2,4,5,12,13,14,15,16,17,18,19,21,22,23,25,26,27,32,33],"gpio_input":[2,4,5,12,13,16,17,18,19,21,22,23,25,26,27,32,33,34,35,36,39]}"#;

// ============================================================
// GpioManager
// ============================================================

#[derive(Debug)]
enum GpioError {
    UnsupportedPin(i32),
    Hardware(EspError),
}

impl fmt::Display for GpioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpioError::UnsupportedPin(pin) => {
                write!(f, "Unsupported GPIO pin {}", pin)
            }
            GpioError::Hardware(e) => write!(f, "GPIO hardware error: {}", e),
        }
    }
}

impl std::error::Error for GpioError {}

/// Quản lý 3 nhóm GPIO riêng biệt để tránh lỗi ownership (moved value)
struct GpioManager {
    // 2 chân chỉ Output (14, 15)
    outputs: Vec<(u8, PinDriver<'static, Output>), 2>,

    // 4 chân chỉ Input (34, 35, 36, 39)
    inputs: Vec<(u8, PinDriver<'static, Input>), 4>,

    // 17 chân vừa Input vừa Output
    inouts: Vec<(u8, PinDriver<'static, InputOutput>), 17>,
}

impl GpioManager {
    fn new() -> Self {
        Self {
            outputs: Vec::new(),
            inputs: Vec::new(),
            inouts: Vec::new(),
        }
    }

    fn add_output(
        &mut self,
        number: u8,
        mut driver: PinDriver<'static, Output>,
    ) -> anyhow::Result<()> {
        driver.set_low()?;

        self.outputs
            .push((number, driver))
            .map_err(|_| anyhow::anyhow!("GpioManager output storage full"))?;

        Ok(())
    }

    fn add_input(
        &mut self,
        number: u8,
        driver: PinDriver<'static, Input>,
    ) -> anyhow::Result<()> {
        self.inputs
            .push((number, driver))
            .map_err(|_| anyhow::anyhow!("GpioManager input storage full"))?;

        Ok(())
    }

    fn add_inout(
        &mut self,
        number: u8,
        mut driver: PinDriver<'static, InputOutput>,
    ) -> anyhow::Result<()> {
        driver.set_low()?;

        self.inouts
            .push((number, driver))
            .map_err(|_| anyhow::anyhow!("GpioManager inout storage full"))?;

        Ok(())
    }

    fn write(&mut self, pin: i32, value: i32) -> Result<(), GpioError> {
        let level = Level::from(value != 0);

        // Tìm trong nhóm chỉ Output
        if let Some((_, driver)) = self
            .outputs
            .iter_mut()
            .find(|(num, _)| *num as i32 == pin)
        {
            return driver
                .set_level(level)
                .map_err(GpioError::Hardware);
        }

        // Tìm trong nhóm InOut
        if let Some((_, driver)) = self
            .inouts
            .iter_mut()
            .find(|(num, _)| *num as i32 == pin)
        {
            return driver
                .set_level(level)
                .map_err(GpioError::Hardware);
        }

        Err(GpioError::UnsupportedPin(pin))
    }

    fn read(&self, pin: i32) -> Result<u8, GpioError> {
        // Tìm trong nhóm chỉ Input
        if let Some((_, driver)) = self
            .inputs
            .iter()
            .find(|(num, _)| *num as i32 == pin)
        {
            return Ok(if driver.is_high() { 1 } else { 0 });
        }

        // Tìm trong nhóm InOut
        if let Some((_, driver)) = self
            .inouts
            .iter()
            .find(|(num, _)| *num as i32 == pin)
        {
            return Ok(if driver.is_high() { 1 } else { 0 });
        }

        Err(GpioError::UnsupportedPin(pin))
    }

    // Gộp toàn bộ 21 chân có chức năng Input để trả về HTTP
    fn read_all_inputs(&self) -> Vec<(u8, u8), 21> {
        let mut res = Vec::new();

        for (pin, driver) in self.inputs.iter() {
            let _ = res.push((*pin, if driver.is_high() { 1 } else { 0 }));
        }

        for (pin, driver) in self.inouts.iter() {
            let _ = res.push((*pin, if driver.is_high() { 1 } else { 0 }));
        }

        // Sắp xếp lại theo thứ tự chân cho đẹp JSON
        res.sort_unstable_by_key(|&(pin, _)| pin);

        res
    }
}

// ============================================================
// Main
// ============================================================

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("========================================");
    info!("ZeroClaw ESP32 Classic starting...");
    info!("========================================");

    let peripherals = Peripherals::take()?;
    let pins = peripherals.pins;

    // --------------------------------------------------------
    // GPIO (GpioManager)
    // --------------------------------------------------------

    let mut gpio_manager = GpioManager::new();

    macro_rules! register_out {
        ($num:expr, $pin:expr) => {
            gpio_manager.add_output($num, PinDriver::output($pin)?)?;
        };
    }

    macro_rules! register_in {
        ($num:expr, $pin:expr) => {
            gpio_manager.add_input(
                $num,
                PinDriver::input($pin, Pull::Floating)?,
            )?;
        };
    }

    macro_rules! register_inout {
        ($num:expr, $pin:expr) => {
            gpio_manager.add_inout(
                $num,
                PinDriver::input_output($pin, Pull::Floating)?,
            )?;
        };
    }

    // 1. Nhóm chỉ OUTPUT (14, 15)
    register_out!(14, pins.gpio14);
    register_out!(15, pins.gpio15);

    // 2. Nhóm chỉ INPUT (34, 35, 36, 39)
    register_in!(34, pins.gpio34);
    register_in!(35, pins.gpio35);
    register_in!(36, pins.gpio36);
    register_in!(39, pins.gpio39);

    // 3. Nhóm vừa INPUT vừa OUTPUT (17 chân)
    // Đăng ký MỘT LẦN DUY NHẤT để không bị lỗi moved value!
    register_inout!(2, pins.gpio2);
    register_inout!(4, pins.gpio4);
    register_inout!(5, pins.gpio5);
    register_inout!(12, pins.gpio12);
    register_inout!(13, pins.gpio13);
    register_inout!(16, pins.gpio16);
    register_inout!(17, pins.gpio17);
    register_inout!(18, pins.gpio18);
    register_inout!(19, pins.gpio19);
    register_inout!(21, pins.gpio21);
    register_inout!(22, pins.gpio22);
    register_inout!(23, pins.gpio23);
    register_inout!(25, pins.gpio25);
    register_inout!(26, pins.gpio26);
    register_inout!(27, pins.gpio27);
    register_inout!(32, pins.gpio32);
    register_inout!(33, pins.gpio33);

    info!(
        "GpioManager ready: {} out-only, {} in-only, {} in-out pins",
        gpio_manager.outputs.len(),
        gpio_manager.inputs.len(),
        gpio_manager.inouts.len()
    );

    let gpio_manager = Arc::new(Mutex::new(gpio_manager));

    // --------------------------------------------------------
    // UART0
    // --------------------------------------------------------

    let uart_config = UartConfig::new().baudrate(Hertz(115_200));

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

    let mut wifi = BlockingWifi::wrap(wifi, sysloop)?;

    start_wifi_ap(&mut wifi)?;

    let ip_info = wifi.wifi().ap_netif().get_ip_info()?;
    let ap_ip = ip_info.ip.to_string();

    let _http_server = start_http_server(
        Arc::clone(&gpio_manager),
        &ap_ip,
    )?;

    info!("========================================");
    info!("Wi-Fi SoftAP started");
    info!("SSID: {}", WIFI_SSID);
    info!("Password: {}", WIFI_PASSWORD);
    info!("AP IP: {}", ap_ip);
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
                            {
                                let mut gpio_guard = match gpio_manager.lock() {
                                    Ok(guard) => guard,
                                    Err(poisoned) => poisoned.into_inner(),
                                };

                                handle_request(
                                    &line,
                                    &mut gpio_guard,
                                    &mut resp_buf,
                                );
                            }

                            let _ = uart.write(resp_buf.as_bytes());
                            let _ = uart.write(b"\n");

                            line.clear();
                        }
                    } else if line.push(b).is_err() {
                        line.clear();
                    }
                }
            }

            Err(_) => {}
        }
    }
}

// ============================================================
// Wi-Fi SoftAP Setup
// ============================================================

fn start_wifi_ap(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
) -> anyhow::Result<()> {
    info!("Starting Wi-Fi SoftAP...");
    info!("SSID: {}", WIFI_SSID);

    let wifi_configuration = wifi::Configuration::AccessPoint(
        AccessPointConfiguration {
            ssid: WIFI_SSID.try_into().unwrap(),
            ssid_hidden: false,
            auth_method: AuthMethod::WPA2Personal,
            password: WIFI_PASSWORD.try_into().unwrap(),
            channel: WIFI_CHANNEL,
            ..Default::default()
        },
    );

    wifi.set_configuration(&wifi_configuration)?;

    info!("Wi-Fi configuration applied");

    wifi.start()?;

    info!("Wi-Fi driver started");

    wifi.wait_netif_up()?;

    info!("Wi-Fi network interface is up");

    Ok(())
}

// ============================================================
// HTTP server
// ============================================================

fn start_http_server(
    gpio_manager: Arc<Mutex<GpioManager>>,
    ap_ip: &str,
) -> anyhow::Result<EspHttpServer<'static>> {
    info!("Starting HTTP server...");

    let server_config = HttpServerConfiguration {
        http_port: HTTP_PORT,
        ..Default::default()
    };

    let mut server = EspHttpServer::new(&server_config)?;

    // --------------------------------------------------------
    // GET /status
    // --------------------------------------------------------

    let status_ip = ap_ip.to_owned();

    server.fn_handler(
        "/status",
        embedded_svc::http::Method::Get,
        move |request| {
            let mut response = request.into_ok_response()?;

            let mut body = String::<256>::new();

            let _ = core::fmt::Write::write_fmt(
                &mut body,
                format_args!(
                    r#"{{"status":"ok","device":"esp32-classic","wifi_mode":"softap","ip":"{}"}}"#,
                    status_ip
                ),
            );

            response.write_all(body.as_bytes())?;

            Ok::<(), anyhow::Error>(())
        },
    )?;

    // --------------------------------------------------------
    // GET /gpio/read
    // --------------------------------------------------------

    let gpio_http_read = Arc::clone(&gpio_manager);

    server.fn_handler(
        "/gpio/read",
        embedded_svc::http::Method::Get,
        move |request| {
            let uri = request.uri();

            let mut pin_val: Option<i32> = None;

            if let Some(query_idx) = uri.find('?') {
                let query = &uri[query_idx + 1..];

                for param in query.split('&') {
                    let mut parts = param.split('=');

                    if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                        if k == "pin" {
                            pin_val = v.parse::<i32>().ok();
                            break;
                        }
                    }
                }
            }

            let manager = gpio_http_read
                .lock()
                .map_err(|_| anyhow::anyhow!("GpioManager lock failed"))?;

            if let Some(pin) = pin_val {
                // Đọc 1 chân
                match manager.read(pin) {
                    Ok(value) => {
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
                    }

                    Err(GpioError::UnsupportedPin(_)) => {
                        let mut response = request.into_response(
                            400,
                            Some("Bad Request"),
                            &[("Content-Type", "application/json")],
                        )?;

                        response.write_all(
                            br#"{"status":"error","message":"Unsupported GPIO input pin"}"#,
                        )?;
                    }

                    Err(e) => return Err(e.into()),
                }
            } else {
                // Đọc TẤT CẢ các chân có tính năng Input (21 chân)
                let all_inputs = manager.read_all_inputs();

                let mut response = request.into_ok_response()?;

                let mut result = String::<1024>::new();

                let _ = core::fmt::Write::write_str(
                    &mut result,
                    r#"{"status":"ok","gpio":{"#,
                );

                for (i, (pin, val)) in all_inputs.iter().enumerate() {
                    if i > 0 {
                        let _ = core::fmt::Write::write_str(
                            &mut result,
                            ",",
                        );
                    }

                    let _ = core::fmt::Write::write_fmt(
                        &mut result,
                        format_args!(r#""{}":{}"#, pin, val),
                    );
                }

                let _ = core::fmt::Write::write_str(
                    &mut result,
                    "}}",
                );

                response.write_all(result.as_bytes())?;
            }

            Ok::<(), anyhow::Error>(())
        },
    )?;

    // --------------------------------------------------------
    // POST /gpio/write
    // --------------------------------------------------------

    let gpio_http_write = Arc::clone(&gpio_manager);

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

                let read_len =
                    request.read(&mut body[total..]).unwrap_or(0);

                if read_len == 0 {
                    break;
                }

                total += read_len;
            }

            let body_str = core::str::from_utf8(&body[..total])
                .map_err(|_| anyhow::anyhow!("Invalid UTF-8"))?;

            info!("GPIO HTTP body: {}", body_str);

            let pin = parse_json_i32(body_str, "pin")
                .ok_or_else(|| anyhow::anyhow!("Missing pin"))?;

            let value = parse_json_i32(body_str, "value")
                .ok_or_else(|| anyhow::anyhow!("Missing value"))?;

            if value != 0 && value != 1 {
                anyhow::bail!("value must be 0 or 1");
            }

            let write_result = {
                let mut manager = gpio_http_write
                    .lock()
                    .map_err(|_| anyhow::anyhow!("GpioManager lock failed"))?;

                manager.write(pin, value)
            };

            match write_result {
                Ok(()) => {}

                Err(GpioError::UnsupportedPin(_)) => {
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

                Err(e) => return Err(e.into()),
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
    info!("GET /gpio/read registered");
    info!("POST /gpio/write registered");

    Ok(server)
}

// ============================================================
// Minimal JSON parser
// ============================================================

fn parse_json_i32(body: &str, key: &str) -> Option<i32> {
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

    let value_part = after_key[colon_pos + 1..].trim_start();

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

fn handle_request(
    line: &[u8],
    gpio: &mut GpioManager,
    resp_buf: &mut String<256>,
) {
    let mut id_buf = [0u8; 32];

    let id_len = copy_id(line, &mut id_buf);

    let id_str =
        core::str::from_utf8(&id_buf[..id_len]).unwrap_or("0");

    match Command::from_line(line) {
        Some(Command::Capabilities) => {
            write_ok(resp_buf, id_str, CAPABILITIES_RESULT);
        }

        Some(Command::GpioRead { pin }) => {
            match gpio.read(pin) {
                Ok(value) => {
                    let mut value_buf: String<8> = String::new();

                    let _ = core::fmt::Write::write_fmt(
                        &mut value_buf,
                        format_args!("{value}"),
                    );

                    write_ok(resp_buf, id_str, &value_buf);
                }

                Err(e) => {
                    write_err(resp_buf, id_str, &e.to_string());
                }
            }
        }

        Some(Command::GpioWrite { pin, value }) => {
            match gpio.write(pin, value) {
                Ok(()) => {
                    write_ok(resp_buf, id_str, "done");
                }

                Err(e) => {
                    write_err(resp_buf, id_str, &e.to_string());
                }
            }
        }

        Some(Command::Ping) => {
            write_ok(resp_buf, id_str, "pong");
        }

        None => {
            write_err(resp_buf, id_str, "Unknown command");
        }
    }
}

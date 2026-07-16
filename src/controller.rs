use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const SONY_VENDOR_ID: u16 = 0x054c;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub path: String,
    pub product: String,
}

pub fn devices() -> Result<Vec<DeviceInfo>> {
    let api = HidApi::new().context("failed to initialize hidapi")?;
    let devices = api
        .device_list()
        .filter(|device| device.vendor_id() == SONY_VENDOR_ID)
        .map(|device| DeviceInfo {
            vendor_id: device.vendor_id(),
            product_id: device.product_id(),
            path: device.path().to_string_lossy().to_string(),
            product: device.product_string().unwrap_or("(unknown)").to_string(),
        })
        .collect();

    Ok(devices)
}

pub fn listen_mic_button_until(
    stop: Option<Arc<AtomicBool>>,
    on_press: &mut impl FnMut() -> Result<bool>,
) -> Result<()> {
    let api = HidApi::new().context("failed to initialize hidapi")?;
    let device = open_dualsense(&api)?;
    let mut was_pressed = false;
    let mut last_press = Instant::now() - Duration::from_secs(1);
    let mut report_count = 0_u64;
    let mut last_report_id = None;
    let mut output_seq = 0_u8;

    println!("Listening for documented DualSense mic button. Press Ctrl-C to stop.");
    println!("USB reports use byte 10 mask 0x04; Bluetooth full reports use byte 11 mask 0x04.");

    loop {
        if should_stop(&stop) {
            return Ok(());
        }

        let Some(report) = read_report(&device)? else {
            continue;
        };
        report_count += 1;

        if last_report_id != Some(report[0]) {
            last_report_id = Some(report[0]);
            println!("Controller report id changed to 0x{:02x}", report[0]);
        }

        let Some(mic) = mic_button_state(&report) else {
            if report_count % 200 == 0 {
                println!("Ignoring unsupported report id 0x{:02x}", report[0]);
            }
            continue;
        };

        if mic.pressed != was_pressed {
            println!(
                "Mic button {} via {} byte {} value 0x{:02x}",
                if mic.pressed { "pressed" } else { "released" },
                mic.transport,
                mic.byte,
                mic.value
            );
        }

        let now = Instant::now();
        if mic.pressed && !was_pressed {
            println!("Triggering Discord mute toggle...");
            let muted = on_press()?;
            sync_mic_led(&device, &report, muted, &mut output_seq);
            println!("Discord mute toggle finished.");
            last_press = now;
        } else if mic.pressed
            && was_pressed
            && now.duration_since(last_press) > Duration::from_millis(800)
        {
            println!("Mic button still reads pressed after 800ms; resetting edge detector.");
            was_pressed = false;
            continue;
        }

        was_pressed = mic.pressed;
    }
}

pub fn listen_mic_button_hold_until(
    stop: Option<Arc<AtomicBool>>,
    on_change: &mut impl FnMut(bool) -> Result<()>,
) -> Result<()> {
    let api = HidApi::new().context("failed to initialize hidapi")?;
    let device = open_dualsense(&api)?;
    let mut was_pressed = false;
    let mut report_count = 0_u64;
    let mut last_report_id = None;

    println!("Listening for documented DualSense mic button hold. Press Ctrl-C to stop.");
    println!("Pressing the mic button sends key down; releasing it sends key up.");

    loop {
        if should_stop(&stop) {
            return Ok(());
        }

        let Some(report) = read_report(&device)? else {
            continue;
        };
        report_count += 1;

        if last_report_id != Some(report[0]) {
            last_report_id = Some(report[0]);
            println!("Controller report id changed to 0x{:02x}", report[0]);
        }

        let Some(mic) = mic_button_state(&report) else {
            if report_count % 200 == 0 {
                println!("Ignoring unsupported report id 0x{:02x}", report[0]);
            }
            continue;
        };

        if mic.pressed && !was_pressed {
            println!(
                "Mic button pressed via {} byte {} value 0x{:02x}",
                mic.transport, mic.byte, mic.value
            );
            on_change(true)?;
        } else if !mic.pressed && was_pressed {
            println!(
                "Mic button released via {} byte {} value 0x{:02x}",
                mic.transport, mic.byte, mic.value
            );
            on_change(false)?;
        }

        was_pressed = mic.pressed;
    }
}

fn should_stop(stop: &Option<Arc<AtomicBool>>) -> bool {
    stop.as_ref()
        .is_some_and(|stop| stop.load(Ordering::Relaxed))
}

pub fn test_mic_led(muted: bool) -> Result<()> {
    let api = HidApi::new().context("failed to initialize hidapi")?;
    let device = open_dualsense(&api)?;
    println!(
        "Testing controller mic LED {}. This does not touch Discord.",
        if muted { "on" } else { "off" }
    );

    let mut output_seq = 0_u8;
    loop {
        let Some(report) = read_report(&device)? else {
            continue;
        };

        if mic_button_state(&report).is_some() {
            set_mic_mute_led(&device, report[0], muted, &mut output_seq)?;
            return Ok(());
        }
    }
}

fn sync_mic_led(device: &HidDevice, input_report: &[u8; 128], muted: bool, output_seq: &mut u8) {
    match set_mic_mute_led(device, input_report[0], muted, output_seq) {
        Ok(()) => println!(
            "Controller mic LED set to {}.",
            if muted { "on" } else { "off" }
        ),
        Err(err) => println!("Warning: could not update controller mic LED: {err}"),
    }
}

fn set_mic_mute_led(
    device: &HidDevice,
    input_report_id: u8,
    muted: bool,
    output_seq: &mut u8,
) -> Result<()> {
    match input_report_id {
        0x01 => write_usb_mic_led_report(device, muted),
        0x31 => write_bluetooth_mic_led_report(device, muted, output_seq),
        other => bail!("unsupported input report id 0x{other:02x} for mic LED output"),
    }
}

fn write_usb_mic_led_report(device: &HidDevice, muted: bool) -> Result<()> {
    let mut report = [0_u8; 63];
    report[0] = 0x02;
    report[2] = 0x03;
    report[9] = u8::from(muted);
    report[10] = if muted { 0x10 } else { 0x00 };

    let written = device
        .write(&report)
        .context("failed to write USB DualSense mic LED report")?;
    println!(
        "USB LED output report wrote {written}/{} bytes.",
        report.len()
    );
    Ok(())
}

fn write_bluetooth_mic_led_report(
    device: &HidDevice,
    muted: bool,
    output_seq: &mut u8,
) -> Result<()> {
    let mut report = [0_u8; 78];
    report[0] = 0x31;
    report[1] = *output_seq << 4;
    report[2] = 0x10;
    report[4] = 0x03;
    report[11] = u8::from(muted);
    report[12] = if muted { 0x10 } else { 0x00 };

    *output_seq = (*output_seq).wrapping_add(1) & 0x0f;

    let crc = dualsense_bluetooth_crc32(&report[..74]);
    report[74..78].copy_from_slice(&crc.to_le_bytes());

    println!(
        "BT LED report seq=0x{:02x} flag1=0x{:02x} led={} power=0x{:02x} crc=0x{:08x}",
        report[1], report[4], report[11], report[12], crc
    );
    let written = device
        .write(&report)
        .context("failed to write Bluetooth DualSense mic LED report")?;
    println!(
        "BT LED output report wrote {written}/{} bytes.",
        report.len()
    );
    Ok(())
}

fn dualsense_bluetooth_crc32(report_without_crc: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff;
    crc = crc32_update(crc, &[0xa2]);
    crc = crc32_update(crc, report_without_crc);
    !crc
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }

    crc
}

struct MicButtonState {
    pressed: bool,
    byte: usize,
    value: u8,
    transport: &'static str,
}

fn mic_button_state(report: &[u8; 128]) -> Option<MicButtonState> {
    match report[0] {
        0x01 => Some(MicButtonState {
            pressed: report[10] & 0x04 != 0,
            byte: 10,
            value: report[10],
            transport: "USB report 0x01",
        }),
        // Bluetooth full report 0x31: Linux's DualSense parser starts the common
        // report at data[2], so buttons[2] is byte 11.
        0x31 => Some(MicButtonState {
            pressed: report[11] & 0x04 != 0,
            byte: 11,
            value: report[11],
            transport: "Bluetooth report 0x31",
        }),
        _ => None,
    }
}

fn open_dualsense(api: &HidApi) -> Result<HidDevice> {
    let devices = api
        .device_list()
        .filter(|device| device.vendor_id() == SONY_VENDOR_ID)
        .collect::<Vec<_>>();

    if devices.is_empty() {
        bail!("no Sony HID device found; connect the DualSense over USB or Bluetooth");
    }

    let device = devices
        .iter()
        .find(|device| {
            device
                .product_string()
                .map(|name| name.to_ascii_lowercase().contains("dualsense"))
                .unwrap_or(false)
        })
        .or_else(|| devices.first())
        .expect("devices is not empty");

    println!(
        "Using controller: vendor=0x{:04x} product=0x{:04x} product={}",
        device.vendor_id(),
        device.product_id(),
        device.product_string().unwrap_or("(unknown)")
    );

    device
        .open_device(api)
        .context("failed to open controller HID device")
}

fn read_report(device: &HidDevice) -> Result<Option<[u8; 128]>> {
    let mut report = [0_u8; 128];
    match device.read_timeout(&mut report, 250) {
        Ok(0) => {
            thread::sleep(Duration::from_millis(10));
            Ok(None)
        }
        Ok(_) => Ok(Some(report)),
        Err(err) => Err(err).context("failed to read controller HID report"),
    }
}

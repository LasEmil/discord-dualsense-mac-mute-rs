use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const SONY_VENDOR_ID: u16 = 0x054c;
/// Reading this feature report is what kicks a Bluetooth DualSense into
/// sending full 0x31 input reports. See `enable_full_report_mode`.
const DUALSENSE_CALIBRATION_FEATURE_REPORT: u8 = 0x05;
/// How long to wait before re-scanning for a controller that isn't there yet.
const RECONNECT_DELAY: Duration = Duration::from_millis(750);
/// A connected DualSense streams reports continuously, even sitting idle. Going
/// this long without one means it is gone even if no read has errored yet —
/// which covers a Bluetooth link dropping silently rather than reporting a
/// disconnect.
const REPORT_STALE_AFTER: Duration = Duration::from_secs(5);
/// Only repeat the "still waiting for a controller" line every N attempts, so
/// an unplugged controller doesn't flood the log.
const WAITING_LOG_EVERY: u32 = 20;

/// Callbacks the mic-button listener fires as the controller comes and goes.
pub trait MicButtonHandler {
    /// The mic button was pressed. Returns the resulting mute state.
    fn on_press(&mut self) -> Result<bool>;

    /// A controller was (re)connected. Returns the mute state to restore to the
    /// controller's mic LED, which comes back off after every reconnect.
    fn on_connected(&mut self) -> Option<bool>;

    /// The controller went away. The listener keeps running and will reconnect.
    fn on_disconnected(&mut self);

    /// No controller could be opened, with the reason why.
    ///
    /// Worth reporting rather than only logging: "no device found" and "found
    /// it but could not open it" both leave the listener waiting, and only the
    /// second one is the user's to fix.
    fn on_waiting(&mut self, reason: &str);
}

/// Why an individual controller session ended.
enum SessionEnd {
    /// The controller went away; reconnect.
    Disconnected,
    /// The caller asked us to stop.
    Stopped,
}

/// Watches the mic button until `stop` is set, surviving any number of
/// controller disconnect/reconnect cycles.
///
/// The outer loop owns (re)connection, the inner session owns one controller.
/// Nothing here treats a vanished controller as fatal: a disconnect ends the
/// session and drops us back into the connect loop.
pub fn listen_mic_button_until(
    stop: Option<Arc<AtomicBool>>,
    handler: &mut impl MicButtonHandler,
) -> Result<()> {
    let mut api = HidApi::new().context("failed to initialize hidapi")?;

    println!("Listening for documented DualSense mic button.");
    println!("USB reports use byte 10 mask 0x04; Bluetooth full reports use byte 11 mask 0x04.");

    let mut waiting_attempts = 0_u32;

    loop {
        if should_stop(&stop) {
            return Ok(());
        }

        let device = match open_dualsense(&mut api) {
            Ok(device) => device,
            Err(err) => {
                if waiting_attempts % WAITING_LOG_EVERY == 0 {
                    println!("Waiting for a DualSense to connect: {err}");
                }
                handler.on_waiting(&err.to_string());
                waiting_attempts += 1;
                sleep_unless_stopped(&stop, RECONNECT_DELAY);
                continue;
            }
        };
        waiting_attempts = 0;

        // The controller's mic LED resets on reconnect, so push the mute state
        // we believe Discord is in rather than leaving the LED lying.
        if let Some(muted) = handler.on_connected() {
            restore_mic_led(&device, &stop, muted);
        }

        let outcome = run_session(&device, &stop, handler);
        handler.on_disconnected();

        match outcome {
            SessionEnd::Stopped => return Ok(()),
            SessionEnd::Disconnected => {
                println!("Controller disconnected; waiting for it to come back.");
                sleep_unless_stopped(&stop, RECONNECT_DELAY);
            }
        }
    }
}

/// Runs one controller connection until it stops or disappears.
///
/// Every piece of state here is per-connection and deliberately local, so a
/// reconnect starts clean — `output_seq` in particular seeds the Bluetooth LED
/// report CRC and must not carry across sessions.
fn run_session(
    device: &HidDevice,
    stop: &Option<Arc<AtomicBool>>,
    handler: &mut impl MicButtonHandler,
) -> SessionEnd {
    let mut was_pressed = false;
    let mut last_press = Instant::now() - Duration::from_secs(1);
    let mut report_count = 0_u64;
    let mut last_report_id = None;
    let mut output_seq = 0_u8;
    let mut last_report_at = Instant::now();

    loop {
        if should_stop(stop) {
            return SessionEnd::Stopped;
        }

        // A read error here is the normal way macOS reports a disconnect, so it
        // ends the session instead of killing the listener.
        let report = match read_report(device) {
            Ok(Some((report, len))) => {
                last_report_at = Instant::now();
                (report, len)
            }
            Ok(None) => {
                if last_report_at.elapsed() > REPORT_STALE_AFTER {
                    println!(
                        "No controller reports for {}s; treating the controller as gone.",
                        REPORT_STALE_AFTER.as_secs()
                    );
                    return SessionEnd::Disconnected;
                }
                continue;
            }
            Err(err) => {
                println!("Controller read failed: {err}");
                return SessionEnd::Disconnected;
            }
        };
        let (buffer, len) = report;
        let report = &buffer[..len];
        report_count += 1;

        if last_report_id != Some(report[0]) {
            last_report_id = Some(report[0]);
            println!("Controller report id changed to 0x{:02x}", report[0]);
        }

        let Some(mic) = mic_button_state(report) else {
            if report_count % 200 == 0 {
                warn_unusable_report(report);
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
            // A failed toggle must not take the listener down with it — the
            // handler records the error, we log it and keep watching.
            match handler.on_press() {
                Ok(muted) => {
                    sync_mic_led(device, report, muted, &mut output_seq);
                    println!("Discord mute toggle finished.");
                }
                Err(err) => println!("Discord mute toggle failed, staying alive: {err}"),
            }
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

/// Pushes `muted` to the mic LED right after a (re)connect, before any input
/// report has arrived to tell us which transport we're on. Reads one report
/// first, since the LED output format depends on the input report id.
fn restore_mic_led(device: &HidDevice, stop: &Option<Arc<AtomicBool>>, muted: bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output_seq = 0_u8;

    while Instant::now() < deadline {
        if should_stop(stop) {
            return;
        }

        match read_report(device) {
            Ok(Some((buffer, len))) if mic_button_state(&buffer[..len]).is_some() => {
                sync_mic_led(device, &buffer[..len], muted, &mut output_seq);
                return;
            }
            Ok(_) => continue,
            Err(err) => {
                println!("Warning: could not restore the mic LED after reconnect: {err}");
                return;
            }
        }
    }

    println!("Warning: timed out restoring the mic LED after reconnect.");
}

fn should_stop(stop: &Option<Arc<AtomicBool>>) -> bool {
    stop.as_ref()
        .is_some_and(|stop| stop.load(Ordering::Relaxed))
}

/// Sleeps, but wakes early once `stop` is set, so stopping a listener that is
/// waiting to reconnect doesn't block the joining thread for the full delay.
fn sleep_unless_stopped(stop: &Option<Arc<AtomicBool>>, duration: Duration) {
    const SLICE: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + duration;

    while Instant::now() < deadline {
        if should_stop(stop) {
            return;
        }
        thread::sleep(SLICE.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Explains *why* a report is unusable. "Unsupported report id" alone hides
/// the common Bluetooth case, where the id is one we handle but the report is
/// too short to contain the button.
fn warn_unusable_report(report: &[u8]) {
    match report.first() {
        Some(id @ (0x01 | 0x31)) => println!(
            "Report id 0x{id:02x} is only {} bytes — too short to contain the mic button. \
             The controller is likely stuck in Bluetooth simple mode.",
            report.len()
        ),
        Some(id) => println!("Ignoring unsupported report id 0x{id:02x}"),
        None => println!("Ignoring empty controller report"),
    }
}

fn sync_mic_led(device: &HidDevice, input_report: &[u8], muted: bool, output_seq: &mut u8) {
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

/// Locates the mic-button bit in a report, or `None` if this report can't
/// carry it. The length guards are load-bearing rather than defensive: a
/// Bluetooth controller in simple mode sends report id 0x01 in only 10 bytes,
/// which collides with the USB 0x01 layout that keeps the bit at byte 10.
fn mic_button_state(report: &[u8]) -> Option<MicButtonState> {
    match report.first()? {
        // USB report 0x01: the common report starts at data[1], so buttons[2]
        // is byte 10.
        0x01 if report.len() > 10 => Some(MicButtonState {
            pressed: report[10] & 0x04 != 0,
            byte: 10,
            value: report[10],
            transport: "USB report 0x01",
        }),
        // Bluetooth full report 0x31: Linux's DualSense parser starts the common
        // report at data[2], so buttons[2] is byte 11.
        0x31 if report.len() > 11 => Some(MicButtonState {
            pressed: report[11] & 0x04 != 0,
            byte: 11,
            value: report[11],
            transport: "Bluetooth report 0x31",
        }),
        _ => None,
    }
}

fn open_dualsense(api: &mut HidApi) -> Result<HidDevice> {
    // `HidApi` caches its device list and only rebuilds it here. Without this
    // refresh, a reconnected controller keeps resolving to the stale path from
    // before the disconnect and every reopen fails.
    api.refresh_devices()
        .context("failed to refresh the HID device list")?;

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

    let device = device
        .open_device(api)
        .context("failed to open controller HID device")?;
    enable_full_report_mode(&device);

    Ok(device)
}

/// Switches a Bluetooth DualSense out of simple mode.
///
/// Over Bluetooth the controller boots into a compatibility mode that only
/// sends a 10-byte report 0x01 — sticks and face buttons, no mic button at
/// all. Reading feature report 0x05 (the calibration blob) is the documented
/// side effect that makes it start sending the full 78-byte report 0x31,
/// which is the only report that carries the mic-button bit.
///
/// Over USB the controller already sends the full report, and this read is
/// harmless, so a failure here is logged rather than fatal.
fn enable_full_report_mode(device: &HidDevice) {
    let mut feature = [0_u8; 64];
    feature[0] = DUALSENSE_CALIBRATION_FEATURE_REPORT;

    match device.get_feature_report(&mut feature) {
        Ok(len) => println!("Requested DualSense full report mode ({len}-byte calibration read)."),
        Err(err) => println!(
            "Warning: could not read the DualSense calibration feature report ({err}); \
             a Bluetooth controller may stay in simple mode and never report the mic button."
        ),
    }
}

/// Reads one HID report, returning the buffer *and* how many bytes actually
/// arrived. The length matters: a Bluetooth DualSense in simple mode sends a
/// 10-byte report, and indexing past that silently reads zero padding rather
/// than failing, which looks exactly like "the button is never pressed".
fn read_report(device: &HidDevice) -> Result<Option<([u8; 128], usize)>> {
    let mut report = [0_u8; 128];
    match device.read_timeout(&mut report, 250) {
        Ok(0) => {
            thread::sleep(Duration::from_millis(10));
            Ok(None)
        }
        Ok(len) => Ok(Some((report, len))),
        Err(err) => Err(err).context("failed to read controller HID report"),
    }
}

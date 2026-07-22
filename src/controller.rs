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
/// How often to poll Discord for mute/deafen changes made outside the
/// controller — from the menu bar, or inside Discord itself — so the LED and
/// lightbar don't drift out of sync with reality.
const SYNC_POLL_INTERVAL: Duration = Duration::from_secs(2);

// Lightbar colours mirroring voice state. Kept moderate so the bar reads as a
// clear status light rather than a distraction. Green live, red muted, amber
// deafened.
const LIGHTBAR_LIVE: (u8, u8, u8) = (0, 80, 24);
const LIGHTBAR_MUTED: (u8, u8, u8) = (130, 0, 0);
const LIGHTBAR_DEAFENED: (u8, u8, u8) = (140, 40, 0);

// Motor amplitudes at full (100%) strength, scaled down by the user's rumble
// setting. The firmer buzz confirms muting; the lighter one, going live.
const RUMBLE_STRONG_FULL: u8 = 200;
const RUMBLE_LIGHT_FULL: u8 = 130;

/// A parsed DualSense battery reading, from the same input reports the mic
/// button rides in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Charge level, 0–100. The controller only reports in ~10% steps, so this
    /// is coarse by nature rather than by rounding.
    pub percent: u8,
    pub state: ChargeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Discharging,
    Charging,
    /// On the cable and done charging.
    Full,
    /// The controller reported a charging error or an out-of-range
    /// temperature/voltage; the level, if any, is not trustworthy.
    Unknown,
}

impl ChargeState {
    /// The wire/label form used by the API and UI.
    pub fn label(self) -> &'static str {
        match self {
            ChargeState::Discharging => "discharging",
            ChargeState::Charging => "charging",
            ChargeState::Full => "full",
            ChargeState::Unknown => "unknown",
        }
    }
}

/// How the app's voice state is mirrored onto the controller's mic LED and
/// lightbar. Deafen outranks mute, since deafening implies muting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    Live,
    Muted,
    Deafened,
}

impl VoiceState {
    /// The mic-mute LED is lit whenever you can't be heard.
    fn mic_led_on(self) -> bool {
        !matches!(self, VoiceState::Live)
    }

    /// Lightbar colour for this state.
    fn lightbar(self) -> (u8, u8, u8) {
        match self {
            VoiceState::Live => LIGHTBAR_LIVE,
            VoiceState::Muted => LIGHTBAR_MUTED,
            VoiceState::Deafened => LIGHTBAR_DEAFENED,
        }
    }
}

/// Callbacks the mic-button listener fires as the controller comes and goes.
pub trait MicButtonHandler {
    /// The mic button was pressed. Returns the resulting voice state.
    fn on_press(&mut self) -> Result<VoiceState>;

    /// A controller was (re)connected. Returns the voice state to restore to the
    /// controller's mic LED and lightbar, which come back reset after every
    /// reconnect.
    fn on_connected(&mut self) -> Option<VoiceState>;

    /// Periodic poll, a couple of times a second's worth apart. Returns
    /// `Some(state)` when the voice state changed outside the controller (a
    /// mute from the menu bar or from inside Discord) so the LED and lightbar
    /// can be re-synced. `None` when nothing changed.
    fn on_tick(&mut self) -> Option<VoiceState>;

    /// Current rumble confirmation strength, 0–100. Read fresh on each toggle
    /// so a settings change takes effect without restarting the listener; 0
    /// means no buzz.
    fn rumble_strength(&self) -> u8;

    /// Whether the lightbar should mirror the voice state. Read fresh so toggling
    /// it in settings takes effect live; when off, the lightbar is driven dark.
    fn lightbar_enabled(&self) -> bool;

    /// Whether a one-off test buzz has been requested (e.g. from the settings
    /// slider). Consumes the request, so it fires once per ask.
    fn take_rumble_test(&mut self) -> bool;

    /// The controller reported a new battery reading. Fires on the first full
    /// report after a connect and whenever the level or charging state changes.
    fn on_battery(&mut self, battery: Battery);

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

        // The controller's mic LED and lightbar reset on reconnect, so push the
        // voice state we believe Discord is in rather than leaving them lying.
        if let Some(state) = handler.on_connected() {
            restore_feedback(&device, &stop, state, handler.lightbar_enabled());
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
    let mut last_battery: Option<Battery> = None;
    let mut last_tick = Instant::now();
    // What the mic LED and lightbar currently show, so the periodic poll only
    // rewrites them when the voice state actually moves.
    let mut last_synced: Option<VoiceState> = None;
    // Track the lightbar setting so flipping it in settings re-syncs promptly,
    // rather than waiting for the next voice-state change.
    let mut last_lightbar = handler.lightbar_enabled();

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

        // Poll Discord for state changed elsewhere (menu bar, or inside Discord)
        // and re-sync the LED and lightbar when it moved. Done here, where a
        // fresh report tells us the transport, rather than on a timer thread
        // that would have to guess USB vs Bluetooth.
        if last_tick.elapsed() >= SYNC_POLL_INTERVAL {
            last_tick = Instant::now();
            if let Some(state) = handler.on_tick()
                && last_synced != Some(state)
            {
                println!("Voice state now {state:?}; syncing controller LED and lightbar.");
                sync_feedback(device, report[0], state, handler.lightbar_enabled(), &mut output_seq);
                last_synced = Some(state);
            }
        }

        // Honour the lightbar setting being flipped while connected: re-apply
        // the current state so the bar lights up or goes dark right away.
        let lightbar = handler.lightbar_enabled();
        if lightbar != last_lightbar {
            last_lightbar = lightbar;
            if let Some(state) = last_synced {
                sync_feedback(device, report[0], state, lightbar, &mut output_seq);
            }
        }

        // A settings-driven test buzz, so the strength slider can be felt while
        // it's adjusted. Uses the firmer "muted" pattern as the reference.
        if handler.take_rumble_test() {
            rumble_confirm(
                device,
                report[0],
                VoiceState::Muted,
                handler.rumble_strength(),
                &mut output_seq,
            );
        }

        // The battery byte rides in the same full reports as the mic button.
        // Only surface it when it moves: the level steps in ~10% increments, so
        // this stays quiet rather than firing on every report.
        if let Some(battery) = battery_state(report)
            && last_battery != Some(battery)
        {
            last_battery = Some(battery);
            println!(
                "Controller battery {}% ({})",
                battery.percent,
                battery.state.label()
            );
            handler.on_battery(battery);
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
                Ok(state) => {
                    sync_feedback(device, report[0], state, handler.lightbar_enabled(), &mut output_seq);
                    last_synced = Some(state);
                    // A short buzz confirms the toggle without needing to look
                    // at the LED. Only a local button press reaches here, so an
                    // external mute never makes the controller rumble.
                    rumble_confirm(
                        device,
                        report[0],
                        state,
                        handler.rumble_strength(),
                        &mut output_seq,
                    );
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

/// Pushes `state` to the mic LED and lightbar right after a (re)connect, before
/// any input report has arrived to tell us which transport we're on. Reads one
/// report first, since the output format depends on the input report id.
fn restore_feedback(
    device: &HidDevice,
    stop: &Option<Arc<AtomicBool>>,
    state: VoiceState,
    lightbar: bool,
) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output_seq = 0_u8;

    while Instant::now() < deadline {
        if should_stop(stop) {
            return;
        }

        match read_report(device) {
            Ok(Some((buffer, len))) if mic_button_state(&buffer[..len]).is_some() => {
                sync_feedback(device, buffer[0], state, lightbar, &mut output_seq);
                return;
            }
            Ok(_) => continue,
            Err(err) => {
                println!("Warning: could not restore controller feedback after reconnect: {err}");
                return;
            }
        }
    }

    println!("Warning: timed out restoring controller feedback after reconnect.");
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

/// Mirrors `state` onto the controller's mic-mute LED and lightbar in one
/// output report. With `lightbar` off, the LED still tracks the state but the
/// lightbar is driven dark rather than left showing a stale colour.
fn sync_feedback(
    device: &HidDevice,
    input_report_id: u8,
    state: VoiceState,
    lightbar: bool,
    output_seq: &mut u8,
) {
    let led_on = state.mic_led_on();
    let rgb = if lightbar { state.lightbar() } else { (0, 0, 0) };

    match write_feedback_report(device, input_report_id, led_on, rgb, output_seq) {
        Ok(()) => println!("Controller feedback set to {state:?} (lightbar={lightbar})."),
        Err(err) => println!("Warning: could not update controller feedback: {err}"),
    }
}

fn write_feedback_report(
    device: &HidDevice,
    input_report_id: u8,
    led_on: bool,
    rgb: (u8, u8, u8),
    output_seq: &mut u8,
) -> Result<()> {
    match input_report_id {
        0x01 => write_usb_feedback(device, led_on, rgb),
        0x31 => write_bluetooth_feedback(device, led_on, rgb, output_seq),
        other => bail!("unsupported input report id 0x{other:02x} for feedback output"),
    }
}

fn write_usb_feedback(device: &HidDevice, led_on: bool, rgb: (u8, u8, u8)) -> Result<()> {
    let mut report = [0_u8; 63];
    report[0] = 0x02;
    // valid_flag1: mic-mute LED (0x01) + power save (0x02) + lightbar (0x04).
    report[2] = 0x03 | 0x04;

    report[9] = u8::from(led_on);
    report[10] = if led_on { 0x10 } else { 0x00 };

    let (r, g, b) = rgb;
    report[45] = r;
    report[46] = g;
    report[47] = b;

    let written = device
        .write(&report)
        .context("failed to write USB DualSense feedback report")?;
    println!(
        "USB feedback report wrote {written}/{} bytes (led={led_on}, rgb={r},{g},{b}).",
        report.len()
    );
    Ok(())
}

fn write_bluetooth_feedback(
    device: &HidDevice,
    led_on: bool,
    rgb: (u8, u8, u8),
    output_seq: &mut u8,
) -> Result<()> {
    let mut report = [0_u8; 78];
    report[0] = 0x31;
    report[1] = *output_seq << 4;
    report[2] = 0x10;
    // valid_flag1: mic-mute LED (0x01) + power save (0x02) + lightbar (0x04).
    report[4] = 0x03 | 0x04;

    report[11] = u8::from(led_on);
    report[12] = if led_on { 0x10 } else { 0x00 };

    // The Bluetooth common report is shifted two bytes past the USB one, so the
    // lightbar bytes land at 47–49 rather than 45–47.
    let (r, g, b) = rgb;
    report[47] = r;
    report[48] = g;
    report[49] = b;

    *output_seq = (*output_seq).wrapping_add(1) & 0x0f;

    let crc = dualsense_bluetooth_crc32(&report[..74]);
    report[74..78].copy_from_slice(&crc.to_le_bytes());

    let written = device
        .write(&report)
        .context("failed to write Bluetooth DualSense feedback report")?;
    println!(
        "BT feedback report wrote {written}/{} bytes (led={led_on}, rgb={r},{g},{b}).",
        report.len()
    );
    Ok(())
}

/// A brief tactile confirmation of a *local* toggle: two light taps for going
/// live, one firmer buzz for muting or deafening. Amplitudes scale with
/// `strength` (0–100); at 0 the buzz is skipped entirely. Blocks the listen
/// loop for the length of the pulse, which is fine — a toggle already blocked
/// on Discord IPC, and the mic button is read again the moment this returns.
fn rumble_confirm(
    device: &HidDevice,
    input_report_id: u8,
    state: VoiceState,
    strength: u8,
    output_seq: &mut u8,
) {
    let strength = strength.min(100);
    if strength == 0 {
        return;
    }

    let scale = |full: u8| (u16::from(full) * u16::from(strength) / 100) as u8;
    let pulse = |left: u8, right: u8, seq: &mut u8| {
        if let Err(err) = write_rumble_report(device, input_report_id, left, right, seq) {
            println!("Warning: could not send rumble: {err}");
        }
    };

    match state {
        VoiceState::Live => {
            let light = scale(RUMBLE_LIGHT_FULL);
            pulse(0, light, output_seq);
            thread::sleep(Duration::from_millis(55));
            pulse(0, 0, output_seq);
            thread::sleep(Duration::from_millis(45));
            pulse(0, light, output_seq);
            thread::sleep(Duration::from_millis(55));
            pulse(0, 0, output_seq);
        }
        VoiceState::Muted | VoiceState::Deafened => {
            pulse(scale(RUMBLE_STRONG_FULL), 0, output_seq);
            thread::sleep(Duration::from_millis(150));
            pulse(0, 0, output_seq);
        }
    }
}

/// Sets the two rumble motors (`left` is the low-frequency/strong motor). Uses
/// the DS4-compatible vibration path, which the DualSense honours.
fn write_rumble_report(
    device: &HidDevice,
    input_report_id: u8,
    left: u8,
    right: u8,
    output_seq: &mut u8,
) -> Result<()> {
    match input_report_id {
        0x01 => {
            let mut report = [0_u8; 63];
            report[0] = 0x02;
            // valid_flag0: HAPTICS_SELECT (0x02) *and* COMPATIBLE_VIBRATION
            // (0x01). The DualSense drives rumble through its voice-coil
            // actuators, so the select bit is required — with only 0x01 the
            // motors stay silent.
            report[1] = 0x03;
            report[3] = right; // motor_right
            report[4] = left; // motor_left
            device
                .write(&report)
                .context("failed to write USB DualSense rumble report")?;
            Ok(())
        }
        0x31 => {
            let mut report = [0_u8; 78];
            report[0] = 0x31;
            report[1] = *output_seq << 4;
            report[2] = 0x10;
            // valid_flag0: HAPTICS_SELECT (0x02) + COMPATIBLE_VIBRATION (0x01).
            report[3] = 0x03;
            report[5] = right; // motor_right
            report[6] = left; // motor_left

            *output_seq = (*output_seq).wrapping_add(1) & 0x0f;

            let crc = dualsense_bluetooth_crc32(&report[..74]);
            report[74..78].copy_from_slice(&crc.to_le_bytes());

            device
                .write(&report)
                .context("failed to write Bluetooth DualSense rumble report")?;
            Ok(())
        }
        other => bail!("unsupported input report id 0x{other:02x} for rumble output"),
    }
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

/// Extracts the battery reading from a full input report, or `None` if this
/// report can't carry it (wrong id, or a Bluetooth simple-mode report too short
/// to reach the status byte).
///
/// The layout mirrors Linux's `hid-playstation` DualSense parser: the `status`
/// byte sits at offset 52 of the common report, which starts at `data[1]` over
/// USB (id 0x01) and `data[2]` over Bluetooth (id 0x31) — the same +1/+2 shift
/// that puts the mic button at byte 10 vs 11. Its low nibble is the charge
/// level (0–10) and its high nibble is the charging state.
fn battery_state(report: &[u8]) -> Option<Battery> {
    let status = match report.first()? {
        0x01 if report.len() > 53 => report[53],
        0x31 if report.len() > 54 => report[54],
        _ => return None,
    };

    let level = status & 0x0f;
    // The controller reports 0–10; scale to a percent, biasing up by half a
    // step so a fresh "10" reads as 100 and a low "1" isn't a misleading 10.
    let percent = ((u16::from(level) * 10 + 5).min(100)) as u8;

    let (state, percent) = match status >> 4 {
        0x0 => (ChargeState::Discharging, percent),
        0x1 => (ChargeState::Charging, percent),
        0x2 => (ChargeState::Full, 100),
        _ => (ChargeState::Unknown, percent),
    };

    Some(Battery { percent, state })
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

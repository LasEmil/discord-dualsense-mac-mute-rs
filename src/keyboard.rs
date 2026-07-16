use anyhow::{Result, bail};
use std::ffi::c_void;

const RIGHT_OPTION_KEY_CODE: u16 = 61;
const EVENT_SOURCE_STATE_HID_SYSTEM_STATE: u32 = 1;
const EVENT_TAP_HID: u32 = 0;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: u32) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(
        source: *mut c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut c_void;
    fn CGEventPost(tap: u32, event: *mut c_void);
    fn CFRelease(cf: *const c_void);
}

pub struct RightOptionKey {
    held: bool,
}

impl RightOptionKey {
    pub fn new() -> Self {
        Self { held: false }
    }

    pub fn press(&mut self) -> Result<()> {
        if self.held {
            return Ok(());
        }

        post_right_option_event(true)?;
        self.held = true;
        println!("Right Option key down.");
        Ok(())
    }

    pub fn release(&mut self) -> Result<()> {
        if !self.held {
            return Ok(());
        }

        post_right_option_event(false)?;
        self.held = false;
        println!("Right Option key up.");
        Ok(())
    }
}

impl Drop for RightOptionKey {
    fn drop(&mut self) {
        if self.held {
            let _ = post_right_option_event(false);
        }
    }
}

fn post_right_option_event(key_down: bool) -> Result<()> {
    unsafe {
        let source = CGEventSourceCreate(EVENT_SOURCE_STATE_HID_SYSTEM_STATE);
        if source.is_null() {
            bail!("failed to create macOS keyboard event source");
        }

        let event = CGEventCreateKeyboardEvent(source, RIGHT_OPTION_KEY_CODE, key_down);
        if event.is_null() {
            CFRelease(source);
            bail!("failed to create macOS Right Option keyboard event");
        }

        CGEventPost(EVENT_TAP_HID, event);
        CFRelease(event);
        CFRelease(source);
    }

    Ok(())
}

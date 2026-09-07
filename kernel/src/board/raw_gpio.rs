// direct register GPIO for pins esp-hal does not expose
// DIO flash mode frees GPIO12/13; esp-hal 1.0 has no peripheral
// types for GPIO12..17 on ESP32-C3

const GPIO_OUT_W1TS: u32 = 0x6000_4008;
const GPIO_OUT_W1TC: u32 = 0x6000_400C;
const GPIO_ENABLE_W1TS: u32 = 0x6000_4024;
const IO_MUX_BASE: u32 = 0x6000_9000;
const IO_MUX_PIN_STRIDE: u32 = 0x04;

pub struct RawOutputPin {
    mask: u32,
}

impl RawOutputPin {
    // safety: pin must not be in use by flash or another driver
    pub unsafe fn new(pin: u8) -> Self {
        let mask = 1u32 << pin;

        let mux_reg = (IO_MUX_BASE + pin as u32 * IO_MUX_PIN_STRIDE) as *mut u32;

        unsafe {
            // IO_MUX: MCU_SEL[14:12] = 1 selects GPIO function
            let val = mux_reg.read_volatile();
            let val = (val & !(0b111 << 12)) | (1 << 12);
            mux_reg.write_volatile(val);

            // GPIO_FUNCn_OUT_SEL_CFG: 0x80 = simple GPIO output
            let out_sel = (0x6000_4554 + pin as u32 * 4) as *mut u32;
            out_sel.write_volatile(0x80);

            (GPIO_ENABLE_W1TS as *mut u32).write_volatile(mask);
            (GPIO_OUT_W1TS as *mut u32).write_volatile(mask);
        }

        Self { mask }
    }
}

impl embedded_hal::digital::ErrorType for RawOutputPin {
    type Error = core::convert::Infallible;
}

impl embedded_hal::digital::OutputPin for RawOutputPin {
    #[inline]
    fn set_high(&mut self) -> Result<(), Self::Error> {
        unsafe {
            (GPIO_OUT_W1TS as *mut u32).write_volatile(self.mask);
        }
        Ok(())
    }

    #[inline]
    fn set_low(&mut self) -> Result<(), Self::Error> {
        unsafe {
            (GPIO_OUT_W1TC as *mut u32).write_volatile(self.mask);
        }
        Ok(())
    }
}

// RTC_CNTL registers that keep digital pads at their last level
// through deep sleep (esp-idf gpio_hold_en / gpio_deep_sleep_hold_en).
// esp-hal 1.0 exposes hold only for the RTC pads (GPIO0..5)
const RTC_CNTL_DIG_ISO: u32 = 0x6000_808C;
const RTC_CNTL_DIG_PAD_HOLD: u32 = 0x6000_80D4;
const DIG_ISO_AUTOHOLD_CLR: u32 = 1 << 10;
const DIG_ISO_AUTOHOLD_EN: u32 = 1 << 11;
const DIG_ISO_FORCE_UNHOLD: u32 = 1 << 14;

/// Freeze `pins` at their current level for the duration of deep
/// sleep. Every other digital pad gets isolated by the sleep entry
/// (esp-hal mirrors esp_sleep_isolate_digital_gpio once auto-hold is
/// enabled), so anything whose level matters while asleep must be
/// listed here.
pub fn hold_through_deep_sleep(pins: &[u8]) {
    let mut mask = 0u32;
    for &p in pins {
        mask |= 1 << p;
    }
    unsafe {
        let hold = RTC_CNTL_DIG_PAD_HOLD as *mut u32;
        hold.write_volatile(hold.read_volatile() | mask);
        let iso = RTC_CNTL_DIG_ISO as *mut u32;
        let v = (iso.read_volatile() & !DIG_ISO_FORCE_UNHOLD) | DIG_ISO_AUTOHOLD_EN;
        iso.write_volatile(v);
    }
}

/// Undo [`hold_through_deep_sleep`] after a wake: held pads keep
/// their frozen level until released, which would override whatever
/// the drivers configure at boot.
pub fn release_deep_sleep_holds() {
    unsafe {
        let hold = RTC_CNTL_DIG_PAD_HOLD as *mut u32;
        hold.write_volatile(0);
        let iso = RTC_CNTL_DIG_ISO as *mut u32;
        let v = (iso.read_volatile() & !DIG_ISO_AUTOHOLD_EN) | DIG_ISO_AUTOHOLD_CLR;
        iso.write_volatile(v);
    }
}

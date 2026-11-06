//! The two capacitive touch pads on the VoCat's head (TOUCH6 on GPIO6, TOUCH7 on GPIO7).
//!
//! esp-hal has no touch sensor driver for the ESP32-S3, so this programs the touch FSM directly,
//! following ESP-IDF's `touch_ll` for the S3: the FSM scans both channels continuously, and the
//! CPU only reads the raw counts. A touch raises the count. [`Pads`] tracks a slowly adapting
//! baseline per pad and detects touches against it with hysteresis.

use embassy_time::{Duration, Timer};
use esp_hal::peripherals::{RTC_CNTL, RTC_IO, SENS};

use crate::PadResources;

/// Charge/discharge time per channel and scan, in 8 MHz cycles (ESP-IDF default).
const MEAS_CYCLES: u16 = 500;
/// Pause between scans, in RTC slow clock cycles (ESP-IDF default).
const SLEEP_CYCLES: u16 = 0xF;
/// Charge current, 0..=7 (ESP-IDF default slope).
const DAC_SLOPE: u8 = 7;
/// A touch is detected when the reading rises this many per mille above the baseline.
///
/// Measured on the VoCat: a finger on a pad gives +50..+85 per mille, while the neighbouring pad
/// picks up +10..+30 per mille through the housing.
const TOUCH_PERMILLE: u32 = 40;
/// A touch ends when the reading falls below this many per mille above the baseline.
const RELEASE_PERMILLE: u32 = 20;
/// The baseline only follows rising readings up to this many per mille above it.
const DRIFT_PERMILLE: u32 = 5;

#[derive(Clone, Copy)]
pub enum Pad {
    /// TOUCH6 on GPIO6, at the back of the cat's head.
    Back,
    /// TOUCH7 on GPIO7, at the front of the cat's head.
    Front,
}

impl Pad {
    /// The touch sensor channel, equal to the GPIO number.
    const fn channel(self) -> usize {
        match self {
            Pad::Back => 6,
            Pad::Front => 7,
        }
    }
}

/// Filtered state of one pad.
#[derive(Clone, Copy, Default)]
struct PadReading {
    /// Smoothed raw count.
    smooth: u32,
    /// Untouched level.
    baseline: u32,
    touched: bool,
}

pub struct Pads {
    _r: PadResources<'static>,
    readings: [PadReading; 2],
}

impl Pads {
    /// Starts the touch sensor and measures the baselines. Don't touch the pads during the
    /// ~200 ms this takes.
    pub async fn new(r: PadResources<'static>) -> Self {
        let sens_regs = SENS::regs();
        let rtc = RTC_CNTL::regs();
        let io = RTC_IO::regs();

        // RTC IO registers are only writable with their clock enabled.
        sens_regs
            .sar_peri_clk_gate_conf()
            .modify(|_, w| w.iomux_clk_en().set_bit());

        // Stop the FSM and select timer mode (touch_ll_set_fsm_mode).
        rtc.touch_ctrl2().modify(|_, w| {
            w.touch_start_fsm_en().set_bit();
            w.touch_start_en().clear_bit();
            w.touch_start_force().clear_bit();
            w.touch_slp_timer_en().clear_bit()
        });
        force_timer_done();

        // SAFETY (all `bits` calls below): the values fit their fields.
        rtc.touch_ctrl1().write(|w| unsafe {
            w.touch_meas_num().bits(MEAS_CYCLES);
            w.touch_sleep_cycles().bits(SLEEP_CYCLES)
        });
        rtc.touch_ctrl2().modify(|_, w| unsafe {
            // Charge between 0.5 V and 2.7 V.
            w.touch_drefh().bits(3);
            w.touch_drefl().bits(0);
            w.touch_drange().bits(3);
            w.touch_xpd_wait().bits(0xFF);
            w.touch_clkgate_en().set_bit()
        });
        rtc.touch_scan_ctrl().modify(|_, w| {
            // Inactive pads are grounded, no denoise channel, no shield pad.
            w.touch_inactive_connection().set_bit();
            w.touch_denoise_en().clear_bit();
            w.touch_shield_pad_en().clear_bit()
        });

        let mut mask = 0u16;
        for pad in [Pad::Back, Pad::Front] {
            let ch = pad.channel();
            // Route the pad to the RTC domain, no pulls, no digital input.
            io.touch_pad(ch).modify(|_, w| unsafe {
                w.mux_sel().set_bit();
                w.fun_sel().bits(0);
                w.fun_ie().clear_bit();
                w.slp_sel().clear_bit();
                w.slp_ie().clear_bit();
                w.slp_oe().clear_bit();
                w.rue().clear_bit();
                w.rde().clear_bit();
                w.tie_opt().clear_bit()
            });
            mask |= 1 << ch;
        }
        rtc.touch_dac().modify(|_, w| unsafe {
            w.touch_pad6_dac().bits(DAC_SLOPE);
            w.touch_pad7_dac().bits(DAC_SLOPE)
        });

        rtc.touch_scan_ctrl().modify(|r, w| unsafe {
            w.touch_scan_pad_map()
                .bits(r.touch_scan_pad_map().bits() | mask)
        });
        sens_regs.sar_touch_conf().modify(|r, w| unsafe {
            w.sar_touch_outen().bits(r.sar_touch_outen().bits() | mask);
            // Read raw data, not the hardware baseline or smoothed value.
            w.sar_touch_data_sel().bits(0)
        });

        // Start the FSM in timer mode (touch_ll_start_fsm).
        force_timer_done();
        rtc.touch_ctrl2()
            .modify(|_, w| w.touch_slp_timer_en().set_bit());

        // Let the FSM finish a few scans, then average a few readings.
        Timer::after(Duration::from_millis(50)).await;
        const SAMPLES: u32 = 8;
        let mut sums = [0u32; 2];
        for _ in 0..SAMPLES {
            Timer::after(Duration::from_millis(20)).await;
            for (sum, pad) in sums.iter_mut().zip([Pad::Back, Pad::Front]) {
                *sum += read_raw(pad);
            }
        }
        Self {
            _r: r,
            readings: sums.map(|sum| PadReading {
                smooth: sum / SAMPLES,
                baseline: sum / SAMPLES,
                touched: false,
            }),
        }
    }

    /// Reads both pads once. Returns a pad that a finger touched since the last call.
    pub fn poll(&mut self) -> Option<Pad> {
        let mut pressed = None;
        for (reading, pad) in self.readings.iter_mut().zip([Pad::Back, Pad::Front]) {
            if update_reading(reading, read_raw(pad)) == Some(true) {
                pressed = Some(pad);
            }
        }
        pressed
    }
}

/// The latest raw count of a pad. Higher means more capacitance.
fn read_raw(pad: Pad) -> u32 {
    // `sar_touch_status(0)` is channel 1; channel 0 has no status register.
    SENS::regs()
        .sar_touch_status(pad.channel() - 1)
        .read()
        .data()
        .bits()
}

fn force_timer_done() {
    let rtc = RTC_CNTL::regs();
    // SAFETY: 3 = force done, 0 = normal; both fit the 2-bit field.
    rtc.touch_ctrl2()
        .modify(|_, w| unsafe { w.touch_timer_force_done().bits(3) });
    rtc.touch_ctrl2()
        .modify(|_, w| unsafe { w.touch_timer_force_done().bits(0) });
}

/// Feeds one raw sample into a pad's filter. Returns the new touch state if it changed.
fn update_reading(r: &mut PadReading, raw: u32) -> Option<bool> {
    r.smooth = (r.smooth * 3 + raw) / 4;

    let above = r.smooth.saturating_sub(r.baseline);
    let was = r.touched;
    if r.touched {
        r.touched = above * 1000 > r.baseline * RELEASE_PERMILLE;
    } else {
        r.touched = above * 1000 > r.baseline * TOUCH_PERMILLE;
    }

    // Follow slow drift (temperature, humidity) only while untouched. Falling
    // readings are followed faster, so a baseline taken during a touch
    // recovers. Rising readings are only followed close to the baseline: a
    // hovering finger or a touch on the other pad raises the count by 10-30
    // per mille without crossing the threshold, and must not drag the
    // baseline up.
    if !r.touched {
        let diff = i64::from(r.smooth) - i64::from(r.baseline);
        let step = if diff < 0 {
            diff / 8
        } else if above * 1000 <= r.baseline * DRIFT_PERMILLE {
            diff / 64
        } else {
            0
        };
        r.baseline = (i64::from(r.baseline) + step) as u32;
    }

    (r.touched != was).then_some(r.touched)
}

//! The 800x480 RGB LCD and the PSRAM framebuffer that it shows.

use core::{alloc::Layout, ops::Range};

use allocator_api2::alloc::Allocator;
use esp_alloc::DmaCompatibleExternalMemory;
#[cfg(feature = "esp32s31")]
use esp_hal::lcd_cam::lcd::DelayMode;
use esp_hal::{
    Blocking,
    dma::aligned::DmaAlignedMut,
    gpio::Level,
    lcd_cam::{
        LcdCam,
        lcd::{
            ClockMode,
            Phase,
            Polarity,
            dpi::{Config, Dpi, Format, FrameTiming},
        },
    },
    time::Rate,
};

use crate::LcdResources;

cfg_select! {
    feature = "esp32s3" => {
        mod bounce_buffers;
        pub use bounce_buffers::Display;
    }
    _ => {
        mod circular;
        pub use circular::Display;
    }
}

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 480;
const FB_BYTES: usize = WIDTH * HEIGHT * 2;

/// Sets up the LCD for the board's panel.
fn connect(r: LcdResources<'static>) -> Dpi<'static, Blocking> {
    let config = Config::default()
        .with_format(Format {
            enable_2byte_mode: true,
            ..Default::default()
        })
        .with_vsync_idle_level(Level::High)
        .with_hsync_idle_level(Level::High)
        .with_de_idle_level(Level::Low)
        .with_disable_black_region(false);

    let config = cfg_select! {
        // esp-bsp's settings for the SUB3 subboard: 18 MHz, 928x548 in total, 35 frames
        // per second. The blanking includes the sync pulses (HSYNC 40 + back porch 40,
        // VSYNC 23 + back porch 32).
        feature = "esp32s3" => config
            .with_clock_mode(ClockMode {
                polarity: Polarity::IdleLow,
                phase: Phase::ShiftHigh,
            })
            .with_frequency(Rate::from_mhz(18))
            .with_timing(FrameTiming {
                horizontal_active_width: WIDTH,
                horizontal_total_width: 928,
                horizontal_blank_front_porch: 80,
                vertical_active_height: HEIGHT,
                vertical_total_height: 548,
                vertical_blank_front_porch: 55,
                hsync_width: 40,
                vsync_width: 23,
                hsync_position: 0,
            }),
        // 14 MHz, 880x530 in total: 30 frames per second. The LCD reads PSRAM directly,
        // and faster clocks underrun when other code uses PSRAM.
        _ => config
            .with_clock_mode(ClockMode {
                polarity: Polarity::IdleLow,
                phase: Phase::ShiftLow,
            })
            .with_frequency(Rate::from_mhz(14))
            .with_timing(FrameTiming {
                horizontal_active_width: WIDTH,
                horizontal_total_width: 880,
                horizontal_blank_front_porch: 32,
                vertical_active_height: HEIGHT,
                vertical_total_height: 530,
                vertical_blank_front_porch: 37,
                hsync_width: 40,
                vsync_width: 23,
                hsync_position: 0,
            })
            // Half a clock of delay on the data keeps vertical lines in the right color.
            .with_output_bit_mode(DelayMode::FallingEdge),
    };

    Dpi::new(LcdCam::new(r.lcd_cam).lcd, r.dma, config)
        .unwrap()
        .with_vsync(r.vsync)
        .with_hsync(r.hsync)
        .with_de(r.de)
        .with_pclk(r.pclk)
        .with_data0(r.data0)
        .with_data1(r.data1)
        .with_data2(r.data2)
        .with_data3(r.data3)
        .with_data4(r.data4)
        .with_data5(r.data5)
        .with_data6(r.data6)
        .with_data7(r.data7)
        .with_data8(r.data8)
        .with_data9(r.data9)
        .with_data10(r.data10)
        .with_data11(r.data11)
        .with_data12(r.data12)
        .with_data13(r.data13)
        .with_data14(r.data14)
        .with_data15(r.data15)
}

/// A black framebuffer in PSRAM, aligned for 64-byte DMA bursts.
fn allocate_framebuffer() -> &'static mut [u8] {
    let layout = Layout::from_size_align(FB_BYTES, 64).unwrap();
    let mut allocation = DmaCompatibleExternalMemory
        .allocate_zeroed(layout)
        .expect("PSRAM framebuffer allocation failed");
    unsafe { allocation.as_mut() }
}

/// Writes the given framebuffer rows back from the data cache to PSRAM, where the
/// DMA reads them.
fn write_back_rows(frame: &mut [u8], rows: Range<usize>) {
    if rows.is_empty() {
        return;
    }
    let bytes = &mut frame[rows.start * WIDTH * 2..rows.end * WIDTH * 2];
    DmaAlignedMut::new(bytes).unwrap().writeback();
}

//! The LCD and the PSRAM framebuffer that it shows.
//!
//! All boards draw on the same 800x480 canvas (see [`crate::shared_canvas`]). A panel shows the
//! part of it from [`ORIGIN`], [`WIDTH`] by [`HEIGHT`] pixels: the whole canvas on the 800x480
//! panels, and a crop on the VoCat's 360x360 panel.

use core::alloc::Layout;

use allocator_api2::alloc::Allocator;
use embedded_graphics::{
    pixelcolor::{IntoStorage, Rgb565},
    prelude::Point,
};
use esp_alloc::DmaCompatibleExternalMemory;

cfg_select! {
    feature = "esp32s3" => {
        mod bounce_buffers;
        mod rgb;
        pub use bounce_buffers::Display;
        use rgb::{connect, write_back_rows};
    }
    feature = "vocat" => {
        mod qspi;
        pub use qspi::Display;
    }
    _ => {
        mod circular;
        mod rgb;
        pub use circular::Display;
        use rgb::{connect, write_back_rows};
    }
}

cfg_select! {
    feature = "vocat" => {
        pub const WIDTH: usize = 360;
        pub const HEIGHT: usize = 360;
        /// The middle of the canvas below the toolbar of the 800x480 boards.
        pub const ORIGIN: Point = Point::new(220, 94);
    }
    _ => {
        pub const WIDTH: usize = 800;
        pub const HEIGHT: usize = 480;
        pub const ORIGIN: Point = Point::zero();
    }
}

const FB_BYTES: usize = WIDTH * HEIGHT * 2;

/// The framebuffer's representation of `color`. The ST77916 takes the high byte first, the RGB
/// panels take the low byte first.
pub fn pixel(color: Rgb565) -> [u8; 2] {
    let raw = color.into_storage();
    if cfg!(feature = "vocat") {
        raw.to_be_bytes()
    } else {
        raw.to_le_bytes()
    }
}

/// A black framebuffer in PSRAM, aligned for 64-byte DMA bursts.
fn allocate_framebuffer() -> &'static mut [u8] {
    let layout = Layout::from_size_align(FB_BYTES, 64).unwrap();
    let mut allocation = DmaCompatibleExternalMemory
        .allocate_zeroed(layout)
        .expect("PSRAM framebuffer allocation failed");
    unsafe { allocation.as_mut() }
}

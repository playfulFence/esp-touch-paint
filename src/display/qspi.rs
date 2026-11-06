//! ESP-VoCat scan-out: the ST77916 controller of the 360x360 round LCD has its own frame memory,
//! and the changed framebuffer rows go to it over QSPI.
//!
//! Every transfer is framed like Espressif's `esp_lcd` QSPI driver does it, with CS held low
//! throughout:
//!
//! 1. a 32-bit header on one data line with DC low: `0x02 0x00 cmd 0x00` for a command, `0x32 0x00
//!    0x2C 0x00` (RAMWR) for pixel data,
//! 2. the command parameters on one line, or the pixels on all four lines, with DC high. Pixels are
//!    RGB565, high byte first.
//!
//! The SPI driver copies the pixels from the PSRAM framebuffer to its DMA buffer in internal RAM.
//! The copy reads through the data cache, so the framebuffer needs no cache writeback.

use core::ops::{Deref, DerefMut, Range};

use esp_hal::{
    Blocking,
    delay::Delay,
    dma_rx_buffer,
    dma_tx_buffer,
    gpio::{Level, Output, OutputConfig},
    spi::{
        Mode,
        master::{Address, Command, Config, DataMode, Spi, SpiDma},
    },
    time::Rate,
};

use super::{HEIGHT, WIDTH, allocate_framebuffer};
use crate::LcdResources;

mod st77916_init;

const ROW_BYTES: usize = WIDTH * 2;
/// Rows per SPI transfer. The SPI peripheral sends at most 32736 bytes at a time.
const CHUNK_ROWS: usize = 45;
const CHUNK: usize = CHUNK_ROWS * ROW_BYTES;
/// The LCD pins go through the GPIO matrix, and 80 MHz garbles the picture.
const SPI_CLOCK: Rate = Rate::from_mhz(40);

const _: () = assert!(CHUNK <= 32736);

const OPCODE_WRITE_CMD: u8 = 0x02;
const OPCODE_WRITE_COLOR: u8 = 0x32;
const CASET: u8 = 0x2a;
const RASET: u8 = 0x2b;
const RAMWR: u8 = 0x2c;
const MADCTL: u8 = 0x36;
const COLMOD: u8 = 0x3a;
const INVON: u8 = 0x21;
const DISPON: u8 = 0x29;

/// The framebuffer that the panel shows.
pub struct Display {
    spi: SpiDma<'static, Blocking>,
    cs: Output<'static>,
    dc: Output<'static>,
    frame: &'static mut [u8],
    // Kept alive: dropping them would switch the panel off.
    _power: Output<'static>,
    _reset: Output<'static>,
    _backlight: Output<'static>,
}

impl Display {
    /// Powers up and initialises the panel, and shows a black framebuffer. This also resets the
    /// touch controller, which shares the reset line.
    pub fn new(r: LcdResources<'static>) -> Self {
        let delay = Delay::new();
        let power = Output::new(r.power, Level::Low, OutputConfig::default());
        let backlight = Output::new(r.backlight, Level::Low, OutputConfig::default());
        let mut reset = Output::new(r.reset, Level::High, OutputConfig::default());
        delay.delay_millis(20);
        reset.set_low();
        delay.delay_millis(120);

        let spi = Spi::new(
            r.spi,
            Config::default()
                .with_frequency(SPI_CLOCK)
                .with_mode(Mode::_0),
        )
        .unwrap()
        .with_sck(r.clk)
        .with_sio0(r.d0)
        .with_sio1(r.d1)
        .with_sio2(r.d2)
        .with_sio3(r.d3)
        .with_dma(r.dma)
        // Only half-duplex writes, but the driver needs a receive buffer too.
        .with_buffers(dma_rx_buffer!(32).unwrap(), dma_tx_buffer!(CHUNK).unwrap());

        let mut display = Self {
            spi,
            cs: Output::new(r.cs, Level::High, OutputConfig::default()),
            dc: Output::new(r.dc, Level::Low, OutputConfig::default()),
            frame: allocate_framebuffer(),
            _power: power,
            _reset: reset,
            _backlight: backlight,
        };

        // RGB order and 16 bits per pixel, then the vendor sequence, which ends with SLPOUT. The
        // BSP inverts the colors once more after it.
        display.command(MADCTL, &[0x00]);
        display.command(COLMOD, &[0x55]);
        for &(command, params, delay_ms) in st77916_init::INIT_SEQUENCE {
            display.command(command, params);
            delay.delay_millis(delay_ms.into());
        }
        display.command(INVON, &[]);
        display.command(DISPON, &[]);

        // The panel's memory holds noise until the first frame.
        display.writeback(0..HEIGHT);
        display._backlight.set_high();
        display
    }

    /// Sends the given framebuffer rows to the panel.
    pub fn writeback(&mut self, rows: Range<usize>) {
        if rows.is_empty() {
            return;
        }
        let [x0_high, x0_low] = 0u16.to_be_bytes();
        let [x1_high, x1_low] = (WIDTH as u16 - 1).to_be_bytes();
        let [y0_high, y0_low] = (rows.start as u16).to_be_bytes();
        let [y1_high, y1_low] = (rows.end as u16 - 1).to_be_bytes();
        self.command(CASET, &[x0_high, x0_low, x1_high, x1_low]);
        self.command(RASET, &[y0_high, y0_low, y1_high, y1_low]);

        self.cs.set_low();
        self.header(OPCODE_WRITE_COLOR, RAMWR);
        for chunk in self.frame[rows.start * ROW_BYTES..rows.end * ROW_BYTES].chunks(CHUNK) {
            self.spi
                .half_duplex_write(DataMode::Quad, Command::None, Address::None, 0, chunk)
                .unwrap();
        }
        self.cs.set_high();
    }

    fn command(&mut self, command: u8, params: &[u8]) {
        self.cs.set_low();
        self.header(OPCODE_WRITE_CMD, command);
        if !params.is_empty() {
            self.spi
                .half_duplex_write(DataMode::Single, Command::None, Address::None, 0, params)
                .unwrap();
        }
        self.cs.set_high();
    }

    /// Sends the 32-bit transfer header on one line with DC low, then raises DC for the
    /// parameters or pixels.
    fn header(&mut self, opcode: u8, command: u8) {
        self.dc.set_low();
        self.spi
            .half_duplex_write(
                DataMode::Single,
                Command::None,
                Address::None,
                0,
                &[opcode, 0x00, command, 0x00],
            )
            .unwrap();
        self.dc.set_high();
    }
}

impl Deref for Display {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.frame
    }
}

impl DerefMut for Display {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.frame
    }
}

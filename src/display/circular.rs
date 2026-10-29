//! ESP32-S31 scan-out: the LCD's DMA reads the PSRAM framebuffer through a circular
//! descriptor chain, so the panel shows every change without a new transfer.

use core::ops::{Deref, DerefMut, Range};

use esp_hal::{
    Blocking,
    dma::{
        BurstConfig,
        DmaDescriptor,
        DmaTxBuffer,
        Owner,
        Preparation,
        aligned::{DmaAlignedMut, InternalMemory},
    },
    lcd_cam::lcd::dpi::DpiTransfer,
    peripherals::AXI_GDMA,
};
use static_cell::ConstStaticCell;

use super::{FB_BYTES, HEIGHT, allocate_framebuffer, connect, write_back_rows};
use crate::LcdResources;

/// The largest multiple of the 64-byte PSRAM burst that fits in a descriptor. The
/// AXI-GDMA must not split a burst across two descriptors.
const CHUNK: usize = 4032;
const CHUNKS: usize = FB_BYTES.div_ceil(CHUNK);
/// The channel of `DMA_AXI_CH0`.
const LCD_CHANNEL: usize = 0;

static DESCRIPTORS: ConstStaticCell<InternalMemory<[DmaDescriptor; CHUNKS]>> =
    ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; CHUNKS]));

/// The framebuffer that the panel shows.
pub struct Display {
    transfer: DpiTransfer<'static, CircularFrameBuf, Blocking>,
}

impl Display {
    /// Starts showing a black framebuffer.
    pub fn new(r: LcdResources<'static>) -> Self {
        let dpi = connect(r);
        let transfer = dpi
            .send(true, CircularFrameBuf::new(allocate_framebuffer()))
            .map_err(|error| error.0)
            .unwrap();

        let channel = AXI_GDMA::regs().out_ch(LCD_CHANNEL);
        // The LCD's reads come before other PSRAM traffic, in 16-byte bursts.
        channel.out_pri().write(|w| unsafe { w.tx_pri().bits(6) });
        channel
            .out_conf0()
            .modify(|_, w| unsafe { w.out_burst_size_sel().bits(1) });
        Self { transfer }
    }

    /// Makes the given framebuffer rows visible to the DMA.
    pub fn writeback(&mut self, rows: Range<usize>) {
        write_back_rows(self.transfer.frame, rows);
    }
}

impl Deref for Display {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.transfer.frame
    }
}

impl DerefMut for Display {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.transfer.frame
    }
}

pub struct CircularFrameBuf {
    descriptors: DmaAlignedMut<'static, [DmaDescriptor]>,
    frame: &'static mut [u8],
}

impl CircularFrameBuf {
    fn new(frame: &'static mut [u8]) -> Self {
        let mut descriptors = DESCRIPTORS.take().get_mut().unsize();
        let base = frame.as_mut_ptr();
        let first = descriptors.as_mut_ptr();
        for (i, descriptor) in descriptors.iter_mut().enumerate() {
            let length = (FB_BYTES - i * CHUNK).min(CHUNK);
            descriptor.set_size(length);
            descriptor.set_length(length);
            descriptor.set_owner(Owner::Dma);
            descriptor.set_suc_eof(false);
            descriptor.buffer = base.wrapping_add(i * CHUNK);
            descriptor.next = first.wrapping_add((i + 1) % CHUNKS);
        }
        Self { descriptors, frame }
    }
}

// SAFETY: the descriptors are static, and the framebuffer is never freed.
unsafe impl DmaTxBuffer for CircularFrameBuf {
    type View = Self;
    type Final = Self;

    fn prepare(&mut self) -> Preparation {
        self.descriptors.writeback();
        write_back_rows(self.frame, 0..HEIGHT);
        Preparation {
            start: self.descriptors.as_mut_ptr(),
            accesses_psram: true,
            burst_transfer: BurstConfig::default(),
            check_owner: Some(false),
            auto_write_back: false,
        }
    }

    fn into_view(self) -> Self::View {
        self
    }

    fn from_view(view: Self::View) -> Self::Final {
        view
    }
}

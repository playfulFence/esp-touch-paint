//! ESP32-S3 scan-out through bounce buffers.
//!
//! The ESP32-S3's LCD buffers only 16 pixels. When its DMA reads the framebuffer from PSRAM, the
//! CPU's PSRAM accesses while drawing make it miss pixels, and the picture shifts to the right.
//! Here the LCD's DMA reads a ring of bounce buffers in internal RAM instead, and a
//! memory-to-memory DMA refills them from the PSRAM framebuffer, like esp-bsp does for this board.
//!
//! The LCD_CAM has no underrun interrupt, so the scan-out starts again from the first bounce
//! buffer at the end of each frame. A late refill can only spoil one frame.

use core::{
    cell::RefCell,
    ops::{Deref, DerefMut, Range},
    ptr::null_mut,
    slice,
    sync::atomic::{Ordering, fence},
};

use critical_section::Mutex;
use esp_hal::{
    Blocking,
    dma::{
        BurstConfig,
        DmaDescriptor,
        DmaRxBuf,
        DmaTxBuf,
        DmaTxBuffer,
        ExternalBurstConfig,
        InternalBurstConfig,
        Mem2Mem,
        Mem2MemRx,
        Mem2MemTx,
        Owner,
        Preparation,
        aligned::DmaAlignedMut,
    },
    handler,
    interrupt::{self, Priority},
    lcd_cam::lcd::dpi::{Dpi, DpiTransfer},
    peripherals::{DMA, Interrupt, LCD_CAM},
    ram,
};
use static_cell::ConstStaticCell;

use super::{FB_BYTES, HEIGHT, WIDTH, allocate_framebuffer, connect, write_back_rows};
use crate::{LcdResources, RefillResources};

const LINE_BYTES: usize = WIDTH * 2;
const BOUNCE_LINES: usize = 10;
const BOUNCE_BYTES: usize = LINE_BYTES * BOUNCE_LINES;
/// A refill has `BUFFERS - 1` bounce buffer times to finish.
const BUFFERS: usize = 3;
/// A multiple of the 64-byte PSRAM burst that fits in one DMA descriptor.
const CHUNK: usize = 3200;
const CHUNKS: usize = BOUNCE_BYTES / CHUNK;
/// The channel of `DMA_CH0`, which the LCD uses.
const LCD_CHANNEL: usize = 0;
/// The channel of `DMA_CH1`, which refills the bounce buffers.
const COPY_CHANNEL: usize = 1;

const COPY_BURST: BurstConfig = BurstConfig {
    external_memory: ExternalBurstConfig::Size64,
    internal_memory: InternalBurstConfig::Enabled,
};

const _: () = assert!(HEIGHT.is_multiple_of(BOUNCE_LINES) && BOUNCE_BYTES.is_multiple_of(CHUNK));
const _: () = assert!(CHUNK.is_multiple_of(64) && BOUNCE_BYTES.is_multiple_of(64));
const _: () = assert!((HEIGHT / BOUNCE_LINES).is_multiple_of(BUFFERS));

static SCAN: Mutex<RefCell<Option<Scan>>> = Mutex::new(RefCell::new(None));

struct Scan {
    frame: *const u8,
    /// The descriptors of the refill source.
    source: *mut DmaDescriptor,
    /// The descriptors of each bounce buffer as a refill target.
    targets: [*mut DmaDescriptor; BUFFERS],
    /// The LCD's ring of descriptors over the bounce buffers.
    ring: *mut DmaDescriptor,
    /// Whether a refill runs.
    busy: bool,
    /// The bounce buffer that the LCD reads.
    reading: usize,
    /// The bounce buffer that the next refill fills.
    next_fill: usize,
    /// Bounce buffers that the LCD has read and that wait for a refill.
    queued: usize,
    /// The first framebuffer line of the next refill.
    next_line: usize,
}

// SAFETY: the pointers lead to static memory and to the leaked framebuffer.
unsafe impl Send for Scan {}

impl Scan {
    /// Ends a refill that is done.
    fn finish(&mut self) {
        let int = DMA::regs().ch(COPY_CHANNEL).in_int();
        if self.busy && int.raw().read().in_suc_eof().bit_is_set() {
            int.clr().write(|w| w.in_suc_eof().clear_bit_by_one());
            self.busy = false;
        }
    }

    /// Ends a refill that is done, and starts the next one if a bounce buffer waits.
    fn pump(&mut self) {
        self.finish();
        if self.queued == 0 || self.busy {
            return;
        }
        let buffer = self.next_fill;
        self.next_fill = (buffer + 1) % BUFFERS;
        self.queued -= 1;
        let lines = self.frame.wrapping_add(self.next_line * LINE_BYTES);
        self.next_line = (self.next_line + BOUNCE_LINES) % HEIGHT;
        start_copy(self.source, self.targets[buffer], lines);
        self.busy = true;
    }

    fn wait_for_copy(&mut self) {
        while self.busy {
            self.finish();
        }
    }

    /// Sets up the refills for a frame that starts from the first bounce buffer. A frame has a
    /// multiple of `BUFFERS` bounce buffers, so the refills that kept up hold the first lines in
    /// all but the last bounce buffer, which the LCD just read.
    fn frame_started(&mut self) {
        self.wait_for_copy();
        self.reading = 0;
        self.next_fill = BUFFERS - 1;
        self.queued = 1;
        self.next_line = (BUFFERS - 1) * BOUNCE_LINES;
        self.pump();
    }

    /// Fills all bounce buffers from the first line while the LCD does not read them.
    fn refill(&mut self) {
        self.wait_for_copy();
        self.reading = 0;
        self.next_fill = 0;
        self.queued = BUFFERS;
        self.next_line = 0;
        while self.queued > 0 {
            self.pump();
            self.wait_for_copy();
        }
    }
}

/// Copies `BOUNCE_BYTES` from `lines` into the bounce buffer of `target`. `start` set up the
/// copy channel.
#[inline(always)]
fn start_copy(source: *mut DmaDescriptor, target: *mut DmaDescriptor, lines: *const u8) {
    for i in 0..CHUNKS {
        // SAFETY: the copy channel is idle, so it does not read the descriptors.
        let (source, target) = unsafe { (&mut *source.add(i), &mut *target.add(i)) };
        source.buffer = lines.wrapping_add(i * CHUNK).cast_mut();
        source.set_length(CHUNK);
        source.set_suc_eof(i == CHUNKS - 1);
        source.set_owner(Owner::Dma);
        target.set_length(0);
        target.set_suc_eof(false);
        target.set_owner(Owner::Dma);
    }
    fence(Ordering::SeqCst);

    let channel = DMA::regs().ch(COPY_CHANNEL);
    channel
        .in_int()
        .clr()
        .write(|w| w.in_suc_eof().clear_bit_by_one());
    channel.in_conf0().modify(|_, w| w.in_rst().set_bit());
    channel.in_conf0().modify(|_, w| w.in_rst().clear_bit());
    channel.out_conf0().modify(|_, w| w.out_rst().set_bit());
    channel.out_conf0().modify(|_, w| w.out_rst().clear_bit());
    channel
        .in_link()
        .modify(|_, w| unsafe { w.inlink_addr().bits(target as u32) });
    channel.in_link().modify(|_, w| w.inlink_start().set_bit());
    channel
        .out_link()
        .modify(|_, w| unsafe { w.outlink_addr().bits(source as u32) });
    channel
        .out_link()
        .modify(|_, w| w.outlink_start().set_bit());
}

/// Links `CHUNKS` descriptors at `descriptors` over `buffer`, `CHUNK` bytes each.
///
/// # Safety
///
/// `descriptors` must point at `CHUNKS` descriptors that no DMA reads.
unsafe fn link(descriptors: *mut DmaDescriptor, buffer: *mut u8) {
    for i in 0..CHUNKS {
        let descriptor = unsafe { &mut *descriptors.add(i) };
        descriptor.set_size(CHUNK);
        descriptor.set_length(CHUNK);
        descriptor.set_suc_eof(i == CHUNKS - 1);
        descriptor.set_owner(Owner::Dma);
        descriptor.buffer = buffer.wrapping_add(i * CHUNK);
        descriptor.next = if i + 1 < CHUNKS {
            descriptors.wrapping_add(i + 1)
        } else {
            null_mut()
        };
    }
}

/// The circular chain over the bounce buffers, as a buffer that `Dpi::send` accepts.
pub struct Ring(*mut DmaDescriptor);

// SAFETY: the chain is in static memory.
unsafe impl Send for Ring {}

unsafe impl DmaTxBuffer for Ring {
    type View = Ring;
    type Final = Ring;

    fn prepare(&mut self) -> Preparation {
        Preparation {
            start: self.0,
            accesses_psram: false,
            burst_transfer: BurstConfig::default(),
            // The chain is circular, so the DMA reads each descriptor many times.
            check_owner: Some(false),
            auto_write_back: false,
        }
    }

    fn into_view(self) -> Self::View {
        self
    }

    fn from_view(view: Self::View) -> Self {
        view
    }
}

/// The framebuffer that the panel shows.
pub struct Display {
    frame: &'static mut [u8],
    _transfer: DpiTransfer<'static, Ring, Blocking>,
    _copier: (Mem2MemRx<'static, Blocking>, Mem2MemTx<'static, Blocking>),
}

impl Display {
    /// Starts showing a black framebuffer. The interrupts run on the calling core.
    pub fn new(lcd: LcdResources<'static>, refill: RefillResources<'static>) -> Self {
        start(connect(lcd), refill, allocate_framebuffer())
    }

    /// Makes the given framebuffer rows visible to the DMA.
    pub fn writeback(&mut self, rows: Range<usize>) {
        write_back_rows(self.frame, rows);
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

/// Starts the scan-out of `frame`, a 64-byte aligned framebuffer in PSRAM.
fn start(
    dpi: Dpi<'static, Blocking>,
    refill: RefillResources<'static>,
    frame: &'static mut [u8],
) -> Display {
    #[repr(C, align(64))]
    struct Memory {
        bounce: [[u8; BOUNCE_BYTES]; BUFFERS],
        ring: [DmaDescriptor; BUFFERS * CHUNKS],
        targets: [[DmaDescriptor; CHUNKS]; BUFFERS],
        source: [DmaDescriptor; CHUNKS],
    }
    static MEMORY: ConstStaticCell<Memory> = ConstStaticCell::new(Memory {
        bounce: [[0; BOUNCE_BYTES]; BUFFERS],
        ring: [DmaDescriptor::EMPTY; BUFFERS * CHUNKS],
        targets: [[DmaDescriptor::EMPTY; CHUNKS]; BUFFERS],
        source: [DmaDescriptor::EMPTY; CHUNKS],
    });

    assert_eq!(frame.len(), FB_BYTES);
    assert!((frame.as_ptr() as usize).is_multiple_of(64));
    write_back_rows(frame, 0..HEIGHT);

    let memory: *mut Memory = MEMORY.take();
    // SAFETY: `memory` is valid, and only these pointers access it from here on.
    let (bounce, ring, targets, source) = unsafe {
        (
            core::array::from_fn::<_, BUFFERS, _>(|i| (&raw mut (*memory).bounce[i]).cast::<u8>()),
            (&raw mut (*memory).ring).cast::<DmaDescriptor>(),
            core::array::from_fn::<_, BUFFERS, _>(|i| {
                (&raw mut (*memory).targets[i]).cast::<DmaDescriptor>()
            }),
            (&raw mut (*memory).source).cast::<DmaDescriptor>(),
        )
    };
    let frame_ptr = frame.as_mut_ptr();

    for i in 0..BUFFERS * CHUNKS {
        // SAFETY: no DMA reads the ring yet.
        let descriptor = unsafe { &mut *ring.add(i) };
        descriptor.set_size(CHUNK);
        descriptor.set_length(CHUNK);
        descriptor.set_suc_eof(false);
        descriptor.set_owner(Owner::Dma);
        descriptor.buffer = bounce[i / CHUNKS].wrapping_add(i % CHUNKS * CHUNK);
        descriptor.next = ring.wrapping_add((i + 1) % (BUFFERS * CHUNKS));
    }

    // One copy through esp-hal sets up the copy channel: the request line, the bursts and the
    // owner checks. The interrupts start the later copies through the registers.
    let copier = Mem2Mem::new(refill.dma, refill.peripheral);
    // SAFETY: the DMA buffers are the only users of this memory until the copy ends.
    let (target, source_buf) = unsafe {
        (
            DmaRxBuf::new_with_config(
                DmaAlignedMut::new(slice::from_raw_parts_mut(targets[0], CHUNKS)).unwrap(),
                DmaAlignedMut::new(slice::from_raw_parts_mut(bounce[0], BOUNCE_BYTES)).unwrap(),
                COPY_BURST,
            )
            .unwrap(),
            DmaTxBuf::new_with_config(
                DmaAlignedMut::new(slice::from_raw_parts_mut(source, CHUNKS)).unwrap(),
                DmaAlignedMut::new(slice::from_raw_parts_mut(frame_ptr, BOUNCE_BYTES)).unwrap(),
                COPY_BURST,
            )
            .unwrap(),
        )
    };
    let rx = copier.rx.receive(target).map_err(|e| e.0).unwrap();
    let tx = copier.tx.send(source_buf).map_err(|e| e.0).unwrap();
    let int = DMA::regs().ch(COPY_CHANNEL).in_int();
    while int.raw().read().in_suc_eof().bit_is_clear() {}
    int.clr().write(|w| w.in_suc_eof().clear_bit_by_one());
    let (_, tx, _) = tx.wait();
    let (rx, _) = rx.stop();
    int.ena().modify(|_, w| w.in_suc_eof().set_bit());

    // SAFETY: the esp-hal buffers are gone, and the copy channel is idle.
    unsafe {
        link(source, null_mut());
        for (descriptors, buffer) in targets.into_iter().zip(bounce) {
            link(descriptors, buffer);
        }
    }

    let mut scan = Scan {
        frame: frame_ptr,
        source,
        targets,
        ring,
        busy: false,
        reading: 0,
        next_fill: 0,
        queued: 0,
        next_line: 0,
    };
    scan.refill();
    critical_section::with(|cs| SCAN.replace(cs, Some(scan)));

    interrupt::bind_handler(Interrupt::DMA_OUT_CH0, lcd_descriptor_done);
    interrupt::bind_handler(Interrupt::DMA_IN_CH1, copy_done);
    interrupt::bind_handler(Interrupt::LCD_CAM, frame_end);
    LCD_CAM::regs()
        .lc_dma_int_ena()
        .modify(|_, w| w.lcd_vsync_int_ena().set_bit());

    let transfer = dpi.send(true, Ring(ring)).map_err(|e| e.0).unwrap();
    let regs = DMA::regs();
    let lcd = regs.ch(LCD_CHANNEL);
    lcd.out_int()
        .clr()
        .write(|w| w.out_done().clear_bit_by_one());
    lcd.out_int().ena().modify(|_, w| w.out_done().set_bit());
    // The LCD's reads come before the refills.
    lcd.out_pri().write(|w| unsafe { w.tx_pri().bits(6) });
    let copy = regs.ch(COPY_CHANNEL);
    copy.out_pri().write(|w| unsafe { w.tx_pri().bits(0) });
    copy.in_pri().write(|w| unsafe { w.rx_pri().bits(0) });

    Display {
        frame,
        _transfer: transfer,
        _copier: (rx, tx),
    }
}

/// Starts the LCD and its DMA again from the first bounce buffer. The pixel clock stops for only
/// a few cycles: the panel blanks when it stops for longer.
#[inline(always)]
fn restart_lcd(ring: *mut DmaDescriptor) {
    let lcd = LCD_CAM::regs();
    let channel = DMA::regs().ch(LCD_CHANNEL);
    lcd.lcd_user().modify(|_, w| w.lcd_start().clear_bit());
    channel.out_link().modify(|_, w| w.outlink_stop().set_bit());
    channel.out_conf0().modify(|_, w| w.out_rst().set_bit());
    channel.out_conf0().modify(|_, w| w.out_rst().clear_bit());
    lcd.lcd_user().modify(|_, w| w.lcd_reset().set_bit());
    lcd.lcd_misc().modify(|_, w| w.lcd_afifo_reset().set_bit());
    channel
        .out_int()
        .clr()
        .write(|w| w.out_done().clear_bit_by_one());
    channel
        .out_link()
        .modify(|_, w| unsafe { w.outlink_addr().bits(ring as u32) });
    channel
        .out_link()
        .modify(|_, w| w.outlink_start().set_bit());
    lcd.lcd_user().modify(|_, w| {
        w.lcd_update().set_bit();
        w.lcd_start().set_bit()
    });
}

/// The LCD's DMA finished a descriptor.
#[handler(priority = Priority::Priority2)]
#[ram]
fn lcd_descriptor_done() {
    let channel = DMA::regs().ch(LCD_CHANNEL);
    channel
        .out_int()
        .clr()
        .write(|w| w.out_done().clear_bit_by_one());
    let current = channel.out_dscr().read().outlink_dscr().bits() as usize;
    critical_section::with(|cs| {
        let mut scan = SCAN.borrow_ref_mut(cs);
        let Some(scan) = scan.as_mut() else {
            return;
        };
        let Some(offset) = current.checked_sub(scan.ring as usize) else {
            return;
        };
        let descriptor = offset / size_of::<DmaDescriptor>();
        if descriptor >= BUFFERS * CHUNKS {
            return;
        }
        let reading = descriptor / CHUNKS;
        // The register can point at the next descriptor while the DMA still sends the last one,
        // so wait until the DMA is past the first descriptor of the next bounce buffer.
        if reading == scan.reading || descriptor.is_multiple_of(CHUNKS) {
            return;
        }
        scan.queued += (reading + BUFFERS - scan.reading) % BUFFERS;
        scan.reading = reading;
        scan.pump();
    });
}

/// The copy DMA finished a refill.
#[handler(priority = Priority::Priority2)]
#[ram]
fn copy_done() {
    critical_section::with(|cs| {
        if let Some(scan) = SCAN.borrow_ref_mut(cs).as_mut() {
            scan.pump();
        }
    });
}

/// The LCD sent the last line of a frame.
#[handler(priority = Priority::Priority2)]
#[ram]
fn frame_end() {
    LCD_CAM::regs()
        .lc_dma_int_clr()
        .write(|w| w.lcd_vsync_int_clr().set_bit());
    critical_section::with(|cs| {
        if let Some(scan) = SCAN.borrow_ref_mut(cs).as_mut() {
            restart_lcd(scan.ring);
            scan.frame_started();
        }
    });
}

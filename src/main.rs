//! Touch-controlled paint program for an 800x480 RGB LCD with a Goodix touch controller.
//!
//! Draw with a finger, choose a color and brush size from the toolbar, use the
//! eraser, or clear the canvas. A board button restores the demo picture.
//!
//! Boards that run this program share the canvas over ESP-NOW: strokes, CLEAR and
//! the restored picture appear on all of them. Each board keeps its own color and
//! brush, and a board that starts later sees only new drawing.

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::{
    assign_resources,
    clock::{ClockConfig, CpuClock},
    ram,
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::{esp_now::BROADCAST_ADDRESS, wifi::WifiController};

use crate::{
    button::RestoreButton,
    display::Display,
    framebuf::FrameBuf,
    shared_canvas::{Op, Outbox, PACKET_BYTES, Peers},
    touch::{Touch, TouchEvent},
    ui::{CANVAS_COLOR, PALETTE, TOOLBAR_HEIGHT, ToolbarButton},
};

mod button;
mod display;
mod framebuf;
mod shared_canvas;
mod touch;
mod ui;

esp_bootloader_esp_idf::esp_app_desc!();

cfg_select! {
    feature = "esp32s3" => {
        // ESP32-S3-LCD-EV-Board with the ESP32-S3-WROOM-1-N16R8 module (board versions
        // before v1.5) and the SUB3 subboard (4.3", 800x480, ST7262 + GT1151).
        assign_resources! {
            pub Resources<'d> {
                lcd: LcdResources<'d> {
                    lcd_cam: LCD_CAM,
                    dma: DMA_CH0,
                    vsync: GPIO3,
                    hsync: GPIO46,
                    de: GPIO17,
                    pclk: GPIO9,
                    data0: GPIO10,
                    data1: GPIO11,
                    data2: GPIO12,
                    data3: GPIO13,
                    data4: GPIO14,
                    data5: GPIO21,
                    data6: GPIO47,
                    data7: GPIO48,
                    data8: GPIO45,
                    data9: GPIO38,
                    data10: GPIO39,
                    data11: GPIO40,
                    data12: GPIO41,
                    data13: GPIO42,
                    data14: GPIO2,
                    data15: GPIO1,
                },
                refill: RefillResources<'d> {
                    dma: DMA_CH1,
                    // The ESP32-S3's memory-to-memory DMA needs a peripheral's request line.
                    peripheral: SPI2,
                },
                touch: TouchResources<'d> {
                    i2c: I2C0,
                    sda: GPIO8,
                    scl: GPIO18,
                },
                button: ButtonResources<'d> {
                    // BOOT
                    pin: GPIO0,
                },
            }
        }
    }
    _ => {
        // ESP32-S31-Korvo-1 with its LCD subboard.
        assign_resources! {
            pub Resources<'d> {
                lcd: LcdResources<'d> {
                    lcd_cam: LCD_CAM,
                    dma: DMA_AXI_CH0,
                    vsync: GPIO45,
                    hsync: GPIO44,
                    de: GPIO43,
                    pclk: GPIO40,
                    data0: GPIO8,
                    data1: GPIO9,
                    data2: GPIO10,
                    data3: GPIO11,
                    data4: GPIO12,
                    data5: GPIO13,
                    data6: GPIO14,
                    data7: GPIO15,
                    data8: GPIO16,
                    data9: GPIO17,
                    data10: GPIO18,
                    data11: GPIO19,
                    data12: GPIO33,
                    data13: GPIO34,
                    data14: GPIO35,
                    data15: GPIO36,
                },
                touch: TouchResources<'d> {
                    i2c: I2C0,
                    sda: GPIO0,
                    scl: GPIO1,
                },
                button: ButtonResources<'d> {
                    // SET, one of the buttons on the ADC ladder
                    pin: GPIO42,
                    adc: ADC1,
                },
            }
        }
    }
}

#[esp_hal::main]
async fn main(_spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(ClockConfig::from(CpuClock::max())),
    );
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 48 * 1024);
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    let timer = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timer.timer0);

    let resources = split_resources!(peripherals);

    let mut selected = 1usize;
    let mut radius = 7i32;
    let mut eraser = false;

    let mut display = cfg_select! {
        feature = "esp32s3" => Display::new(resources.lcd, resources.refill),
        _ => Display::new(resources.lcd),
    };
    let dirty = {
        let mut frame = FrameBuf::new(&mut display);
        frame.fill_rect(
            0,
            0,
            display::WIDTH as i32,
            display::HEIGHT as i32,
            CANVAS_COLOR,
        );
        ui::draw_toolbar(&mut frame, selected, radius, eraser);
        frame.dirty_rows()
    };
    display.writeback(dirty);

    let wifi = WifiController::new(peripherals.WIFI, Default::default()).unwrap();
    let mut esp_now = wifi.esp_now();
    esp_now.set_channel(shared_canvas::CHANNEL).unwrap();
    let mut outbox = Outbox::new(Rng::new().random());
    let mut peers = Peers::new();
    let mut tx_buffer = [0u8; PACKET_BYTES];

    let mut touch = Touch::new(resources.touch).await;
    println!(
        "Touch Paint ready (Goodix at 0x{:02x}), sharing the canvas on ESP-NOW channel {}",
        touch.address(),
        shared_canvas::CHANNEL
    );

    let mut button = RestoreButton::new(resources.button);
    let mut was_down = false;
    let mut last_point: Option<(i32, i32)> = None;
    let mut button_debounce = 0u8;
    let mut button_latched = false;

    loop {
        Timer::after(Duration::from_millis(15)).await;

        let dirty = {
            let mut frame = FrameBuf::new(&mut display);
            while let Some(received) = esp_now.receive() {
                peers.receive(received.info.src_address, received.data(), |op| {
                    apply(&mut frame, op)
                });
            }
            frame.dirty_rows()
        };
        display.writeback(dirty);

        if button.is_pressed() {
            button_debounce = button_debounce.saturating_add(1);
            if button_debounce >= 3 && !button_latched {
                let dirty = {
                    let mut frame = FrameBuf::new(&mut display);
                    apply(&mut frame, Op::Restore);
                    ui::draw_toolbar(&mut frame, selected, radius, eraser);
                    frame.dirty_rows()
                };
                display.writeback(dirty);
                outbox.push(Op::Restore);
                last_point = None;
                button_latched = true;
                println!("Restored demo picture");
            }
        } else {
            button_debounce = 0;
            button_latched = false;
        }

        match touch.read().await {
            None => {}
            Some(TouchEvent::Up) => {
                was_down = false;
                last_point = None;
            }
            Some(TouchEvent::Down(x, y)) => {
                let dirty = {
                    let mut frame = FrameBuf::new(&mut display);
                    if y < TOOLBAR_HEIGHT {
                        if !was_down {
                            match ui::toolbar_button(x) {
                                Some(ToolbarButton::Color(index)) => {
                                    selected = index;
                                    eraser = false;
                                }
                                Some(ToolbarButton::BrushSize(size)) => radius = size,
                                Some(ToolbarButton::Eraser) => eraser = true,
                                Some(ToolbarButton::Clear) => {
                                    apply(&mut frame, Op::Clear);
                                    outbox.push(Op::Clear);
                                }
                                None => {}
                            }
                            ui::draw_toolbar(&mut frame, selected, radius, eraser);
                        }
                        last_point = None;
                    } else {
                        let point = (x, y);
                        let op = Op::Line {
                            from: last_point.unwrap_or(point),
                            to: point,
                            radius,
                            color: if eraser {
                                CANVAS_COLOR
                            } else {
                                PALETTE[selected]
                            },
                        };
                        apply(&mut frame, op);
                        outbox.push(op);
                        last_point = Some(point);
                    }
                    frame.dirty_rows()
                };
                display.writeback(dirty);
                was_down = true;
            }
        }

        if let Some(packet) = outbox.next_packet(&mut tx_buffer) {
            let _ = esp_now.send_async(&BROADCAST_ADDRESS, packet).await;
        }
    }
}

fn apply(frame: &mut FrameBuf<'_>, op: Op) {
    match op {
        Op::Line {
            from,
            to,
            radius,
            color,
        } => frame.draw_line(from, to, radius, color),
        Op::Clear => ui::clear_canvas(frame),
        Op::Restore => ui::draw_demo_picture(frame),
    }
}

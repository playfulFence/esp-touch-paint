//! The touch controller on the I2C bus: a Goodix GT911 or GT1151 on the 800x480 panels, a CST816T
//! on the VoCat.

use esp_hal::{
    Async,
    i2c::master::{Config, I2c},
    time::Rate,
};

use crate::{
    TouchResources,
    display::{HEIGHT, ORIGIN, WIDTH},
};

pub enum TouchEvent {
    Up,
    /// A finger at the given canvas coordinates.
    Down(i32, i32),
}

pub struct Touch {
    i2c: I2c<'static, Async>,
    address: u8,
}

fn connect(r: TouchResources<'static>) -> I2c<'static, Async> {
    I2c::new(r.i2c, Config::default().with_frequency(Rate::from_khz(400)))
        .unwrap()
        .with_sda(r.sda)
        .with_scl(r.scl)
        .into_async()
}

/// The canvas coordinates of a touch at panel coordinates `x` and `y`.
fn down(x: u16, y: u16) -> TouchEvent {
    let x = x.min(WIDTH as u16 - 1) as i32;
    let y = y.min(HEIGHT as u16 - 1) as i32;
    TouchEvent::Down(x + ORIGIN.x, y + ORIGIN.y)
}

impl Touch {
    pub fn address(&self) -> u8 {
        self.address
    }
}

cfg_select! {
    feature = "vocat" => {
        /// The touch report: gesture, number of fingers, X and Y. The coordinates have 12 bits.
        const REPORT: u8 = 0x01;
        /// Interrupt control. 0x60 pulses INT on touch and on movement.
        const IRQ_CTL: u8 = 0xfa;
        /// Writing 1 keeps the controller awake. Asleep, it answers only after a touch.
        const DIS_AUTO_SLEEP: u8 = 0xfe;

        impl Touch {
            /// Sets up the controller. The LCD reset also resets it, and it goes to sleep a few
            /// seconds after that.
            pub async fn new(r: TouchResources<'static>) -> Self {
                let mut touch = Self {
                    i2c: connect(r),
                    address: 0x15,
                };
                for (register, value) in [(DIS_AUTO_SLEEP, 0x01), (IRQ_CTL, 0x60)] {
                    touch
                        .i2c
                        .write_async(touch.address, &[register, value])
                        .await
                        .expect("CST816T touch controller not found");
                }
                touch
            }

            /// Reads the controller.
            pub async fn read(&mut self) -> Option<TouchEvent> {
                let mut report = [0u8; 6];
                self.i2c
                    .write_read_async(self.address, &[REPORT], &mut report)
                    .await
                    .ok()?;
                Some(if report[1] == 0 {
                    TouchEvent::Up
                } else {
                    down(
                        u16::from_be_bytes([report[2] & 0x0f, report[3]]),
                        u16::from_be_bytes([report[4] & 0x0f, report[5]]),
                    )
                })
            }
        }
    }
    _ => {
        /// The status register. Bit 7 tells that new data is ready, bits 0 to 3 count the
        /// touch points. Writing 0 acknowledges the data.
        const STATUS: [u8; 2] = [0x81, 0x4e];
        /// The coordinates of the first touch point.
        const POINT: [u8; 2] = [0x81, 0x50];

        impl Touch {
            /// Finds the controller at either of its I2C addresses.
            pub async fn new(r: TouchResources<'static>) -> Self {
                let mut i2c = connect(r);
                for address in [0x14, 0x5d] {
                    let mut status = [0u8; 1];
                    if i2c
                        .write_read_async(address, &STATUS, &mut status)
                        .await
                        .is_ok()
                    {
                        return Self { i2c, address };
                    }
                }
                panic!("Goodix touch controller not found");
            }

            /// Reads the controller. Returns `None` when it has no new data.
            pub async fn read(&mut self) -> Option<TouchEvent> {
                let mut status = [0u8; 1];
                self.i2c
                    .write_read_async(self.address, &STATUS, &mut status)
                    .await
                    .ok()?;
                if status[0] & 0x80 == 0 {
                    return None;
                }

                let event = if status[0] & 0x0f == 0 {
                    Some(TouchEvent::Up)
                } else {
                    let mut point = [0u8; 4];
                    self.i2c
                        .write_read_async(self.address, &POINT, &mut point)
                        .await
                        .ok()
                        .map(|_| {
                            down(
                                u16::from_le_bytes([point[0], point[1]]),
                                u16::from_le_bytes([point[2], point[3]]),
                            )
                        })
                };

                let _ = self
                    .i2c
                    .write_async(self.address, &[STATUS[0], STATUS[1], 0])
                    .await;
                event
            }
        }
    }
}

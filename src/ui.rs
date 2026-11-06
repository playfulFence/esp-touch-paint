//! The toolbar and the demo picture.
//!
//! The 800x480 boards have a toolbar along the top of the canvas. The VoCat has a row of color
//! dots along the bottom edge of its round panel, a brush preview at the top, and its touch pads
//! for CLEAR and the brush size.

use embedded_graphics::{
    mono_font::{
        MonoTextStyle,
        ascii::{FONT_6X10, FONT_10X20},
    },
    pixelcolor::Rgb565,
    prelude::*,
    text::{Alignment, Text},
};

use crate::{
    framebuf::FrameBuf,
    shared_canvas::{CANVAS_HEIGHT, CANVAS_WIDTH},
};

cfg_select! {
    feature = "esp32s3" => {
        const TITLE: &str = "ESP32-S3 TOUCH PAINT";
        const RESTORE_HINT: &str = "Draw over me - BOOT restores this picture";
    }
    feature = "vocat" => {
        const TITLE: &str = "ESP-VOCAT TOUCH PAINT";
        const RESTORE_HINT: &str = "Draw over me - BOOT restores this picture";
    }
    _ => {
        const TITLE: &str = "ESP32-S31 TOUCH PAINT";
        const RESTORE_HINT: &str = "Draw over me - SET restores this picture";
    }
}

/// The canvas rows above this are the toolbar of the 800x480 boards. Strokes stay below it.
pub const TOOLBAR_HEIGHT: i32 = 68;
pub const CANVAS_COLOR: Rgb565 = Rgb565::WHITE;
pub const PALETTE: [Rgb565; 8] = [
    Rgb565::BLACK,
    Rgb565::RED,
    Rgb565::new(31, 32, 0),
    Rgb565::YELLOW,
    Rgb565::GREEN,
    Rgb565::CYAN,
    Rgb565::BLUE,
    Rgb565::MAGENTA,
];

/// The brush radii: S, M and L.
pub const BRUSH_RADII: [i32; 3] = [3, 7, 13];

const SELECTED_COLOR: Rgb565 = Rgb565::new(0, 55, 31);

pub enum ToolbarButton {
    Color(usize),
    BrushSize(i32),
    Eraser,
    Clear,
}

/// The brush radius after `radius`, for boards that step through the sizes.
#[cfg(feature = "vocat")]
pub fn next_brush_radius(radius: i32) -> i32 {
    let index = BRUSH_RADII.iter().position(|&r| r == radius).unwrap_or(0);
    BRUSH_RADII[(index + 1) % BRUSH_RADII.len()]
}

pub fn clear_canvas(frame: &mut FrameBuf<'_>) {
    frame.fill_rect(
        0,
        TOOLBAR_HEIGHT,
        CANVAS_WIDTH,
        CANVAS_HEIGHT - TOOLBAR_HEIGHT,
        CANVAS_COLOR,
    );
}

cfg_select! {
    feature = "vocat" => {
        use embedded_graphics::primitives::{Circle, PrimitiveStyle};

        use crate::display::{HEIGHT, ORIGIN, WIDTH};

        /// The center of the round panel.
        const CENTER: Point = Point::new(ORIGIN.x + WIDTH as i32 / 2, ORIGIN.y + HEIGHT as i32 / 2);
        /// The palette colors, then the eraser.
        const DOTS: usize = PALETTE.len() + 1;
        const DOT_RADIUS: i32 = 14;
        /// The ring around each dot.
        const RING: i32 = 3;
        const RING_COLOR: Rgb565 = Rgb565::CSS_DIM_GRAY;
        /// The brush preview at the top edge.
        const PREVIEW: Point = Point::new(CENTER.x, ORIGIN.y + 30);
        const PREVIEW_RADIUS: i32 = 15;

        /// The center of dot `index`, on an arc along the bottom edge from left to right.
        fn dot_center(index: usize) -> Point {
            // From 150 to 30 degrees, where 0 is right and y points down.
            let angle = (150.0 - 120.0 * (index as f32 + 0.5) / DOTS as f32).to_radians();
            let distance = (WIDTH as i32 / 2 - DOT_RADIUS - RING - 8) as f32;
            CENTER
                + Point::new(
                    (distance * libm::cosf(angle)) as i32,
                    (distance * libm::sinf(angle)) as i32,
                )
        }

        /// The dot at `x` and `y`, if any.
        fn dot_at(x: i32, y: i32) -> Option<usize> {
            (0..DOTS).find(|&index| {
                let d = Point::new(x, y) - dot_center(index);
                // A bit larger than the dot: fingers are not precise.
                let reach = DOT_RADIUS + RING + 1;
                d.x * d.x + d.y * d.y <= reach * reach
            })
        }

        /// Whether `x` and `y` are on a dot.
        pub fn on_toolbar(x: i32, y: i32) -> bool {
            dot_at(x, y).is_some()
        }

        /// The dot at `x` and `y`, if any.
        pub fn toolbar_button(x: i32, y: i32) -> Option<ToolbarButton> {
            dot_at(x, y).map(|index| {
                if index < PALETTE.len() {
                    ToolbarButton::Color(index)
                } else {
                    ToolbarButton::Eraser
                }
            })
        }

        fn fill_circle(frame: &mut FrameBuf<'_>, center: Point, radius: i32, color: Rgb565) {
            let _ = Circle::with_center(center, (radius * 2 + 1) as u32)
                .into_styled(PrimitiveStyle::with_fill(color))
                .draw(frame);
        }

        fn draw_dot(frame: &mut FrameBuf<'_>, index: usize, selected: usize, eraser: bool) {
            let center = dot_center(index);
            let (color, chosen) = match PALETTE.get(index) {
                Some(&color) => (color, !eraser && selected == index),
                None => (CANVAS_COLOR, eraser),
            };
            fill_circle(
                frame,
                center,
                DOT_RADIUS + RING,
                if chosen { SELECTED_COLOR } else { RING_COLOR },
            );
            fill_circle(frame, center, DOT_RADIUS, color);
        }

        fn draw_preview(frame: &mut FrameBuf<'_>, selected: usize, radius: i32, eraser: bool) {
            fill_circle(frame, PREVIEW, PREVIEW_RADIUS, Rgb565::BLACK);
            let color = if eraser {
                CANVAS_COLOR
            } else {
                PALETTE[selected]
            };
            fill_circle(frame, PREVIEW, radius, color);
        }

        pub fn draw_toolbar(frame: &mut FrameBuf<'_>, selected: usize, radius: i32, eraser: bool) {
            for index in 0..DOTS {
                draw_dot(frame, index, selected, eraser);
            }
            draw_preview(frame, selected, radius, eraser);
        }

        /// Redraws the dots and the preview where drawing on the canvas covered them.
        pub fn repair_toolbar(
            frame: &mut FrameBuf<'_>,
            selected: usize,
            radius: i32,
            eraser: bool,
        ) {
            let reach = DOT_RADIUS + RING;
            for index in 0..DOTS {
                let y = dot_center(index).y;
                if frame.is_dirty(y - reach, y + reach + 1) {
                    draw_dot(frame, index, selected, eraser);
                }
            }
            if frame.is_dirty(PREVIEW.y - PREVIEW_RADIUS, PREVIEW.y + PREVIEW_RADIUS + 1) {
                draw_preview(frame, selected, radius, eraser);
            }
        }
    }
    _ => {
        const TOOLBAR_COLOR: Rgb565 = Rgb565::new(3, 6, 8);
        const TOOLBAR_BORDER: Rgb565 = Rgb565::new(10, 24, 24);

        /// The x position, label and brush radius of the S, M and L buttons.
        const BRUSH_SIZES: [(i32, &str, i32); 3] = [
            (448, "S", BRUSH_RADII[0]),
            (500, "M", BRUSH_RADII[1]),
            (552, "L", BRUSH_RADII[2]),
        ];

        /// Whether `x` and `y` are on the toolbar.
        pub fn on_toolbar(_x: i32, y: i32) -> bool {
            y < TOOLBAR_HEIGHT
        }

        /// The toolbar button at `x` and `y`, if any.
        pub fn toolbar_button(x: i32, _y: i32) -> Option<ToolbarButton> {
            match x {
                ..432 => Some(ToolbarButton::Color(
                    ((x - 8).max(0) / 53).clamp(0, 7) as usize
                )),
                448..=490 => Some(ToolbarButton::BrushSize(BRUSH_SIZES[0].2)),
                491..=542 => Some(ToolbarButton::BrushSize(BRUSH_SIZES[1].2)),
                543..595 => Some(ToolbarButton::BrushSize(BRUSH_SIZES[2].2)),
                608..690 => Some(ToolbarButton::Eraser),
                704.. => Some(ToolbarButton::Clear),
                _ => None,
            }
        }

        pub fn draw_toolbar(frame: &mut FrameBuf<'_>, selected: usize, radius: i32, eraser: bool) {
            frame.fill_rect(0, 0, CANVAS_WIDTH, TOOLBAR_HEIGHT, TOOLBAR_COLOR);
            frame.fill_rect(0, TOOLBAR_HEIGHT - 2, CANVAS_WIDTH, 2, TOOLBAR_BORDER);

            for (index, color) in PALETTE.iter().copied().enumerate() {
                let x = 8 + index as i32 * 53;
                frame.fill_rect(x, 10, 43, 43, color);
                frame.stroke_rect(
                    x - 3,
                    7,
                    49,
                    49,
                    if !eraser && selected == index {
                        SELECTED_COLOR
                    } else {
                        TOOLBAR_BORDER
                    },
                );
            }

            let text = MonoTextStyle::new(&FONT_6X10, Rgb565::WHITE);
            for (x, label, size) in BRUSH_SIZES {
                frame.fill_rect(
                    x,
                    10,
                    43,
                    43,
                    if radius == size {
                        Rgb565::new(0, 24, 20)
                    } else {
                        Rgb565::new(6, 10, 12)
                    },
                );
                frame.stroke_rect(
                    x,
                    10,
                    43,
                    43,
                    if radius == size {
                        SELECTED_COLOR
                    } else {
                        TOOLBAR_BORDER
                    },
                );
                let _ = Text::new(label, Point::new(x + 18, 37), text).draw(frame);
            }

            frame.fill_rect(
                608,
                10,
                82,
                43,
                if eraser {
                    Rgb565::new(18, 14, 3)
                } else {
                    Rgb565::new(6, 10, 12)
                },
            );
            frame.stroke_rect(
                608,
                10,
                82,
                43,
                if eraser {
                    Rgb565::YELLOW
                } else {
                    TOOLBAR_BORDER
                },
            );
            let _ = Text::new("ERASER", Point::new(629, 37), text).draw(frame);

            frame.fill_rect(704, 10, 88, 43, Rgb565::new(18, 4, 4));
            frame.stroke_rect(704, 10, 88, 43, Rgb565::RED);
            let _ = Text::new("CLEAR", Point::new(733, 37), text).draw(frame);
        }

        /// Redraws the parts of the toolbar that drawing on the canvas covered. Strokes stay
        /// below the toolbar, so there are none.
        pub fn repair_toolbar(
            _frame: &mut FrameBuf<'_>,
            _selected: usize,
            _radius: i32,
            _eraser: bool,
        ) {
        }
    }
}

pub fn draw_demo_picture(frame: &mut FrameBuf<'_>) {
    const SKY: Rgb565 = Rgb565::new(8, 42, 31);
    const GRASS: Rgb565 = Rgb565::new(5, 38, 10);
    const MOUNTAIN_BACK: Rgb565 = Rgb565::new(11, 25, 20);
    const MOUNTAIN_FRONT: Rgb565 = Rgb565::new(9, 18, 13);

    frame.fill_rect(
        0,
        TOOLBAR_HEIGHT,
        CANVAS_WIDTH,
        CANVAS_HEIGHT - TOOLBAR_HEIGHT,
        SKY,
    );
    frame.draw_brush(675, 132, 38, Rgb565::YELLOW);

    for (x, y, radius) in [(105, 125, 24), (135, 112, 31), (170, 126, 25)] {
        frame.draw_brush(x, y, radius, Rgb565::WHITE);
    }

    frame.fill_triangle(210, 150, 230, 220, MOUNTAIN_BACK);
    frame.fill_triangle(520, 135, 285, 235, MOUNTAIN_FRONT);
    frame.fill_triangle(520, 135, 55, 46, Rgb565::WHITE);
    frame.fill_triangle(210, 150, 45, 42, Rgb565::WHITE);

    frame.fill_rect(0, 350, CANVAS_WIDTH, 130, GRASS);
    frame.fill_rect(116, 316, 25, 105, Rgb565::new(16, 24, 5));
    for (x, y, radius) in [
        (128, 290, 48),
        (91, 310, 38),
        (164, 308, 40),
        (128, 255, 38),
    ] {
        frame.draw_brush(x, y, radius, Rgb565::new(3, 31, 7));
    }

    frame.fill_rect(270, 378, 330, 60, Rgb565::new(2, 9, 12));
    frame.stroke_rect(270, 378, 330, 60, Rgb565::CYAN);
    let title = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
    let subtitle = MonoTextStyle::new(&FONT_6X10, Rgb565::CYAN);
    let _ = Text::with_alignment(TITLE, Point::new(435, 408), title, Alignment::Center).draw(frame);
    let _ = Text::with_alignment(
        RESTORE_HINT,
        Point::new(435, 427),
        subtitle,
        Alignment::Center,
    )
    .draw(frame);
}

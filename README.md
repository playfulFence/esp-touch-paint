# esp-touch-paint

A finger-paint program for Espressif boards with a touch LCD, written in bare-metal Rust with
[esp-hal](https://github.com/esp-rs/esp-hal).

Boards that run it share the canvas over ESP-NOW: draw on one board, and the strokes appear on all
of them.

![Two boards share one canvas: writing on one board appears on the other](docs/demo.gif)

## Boards

| Board | Feature | Restore button |
| --- | --- | --- |
| [ESP32-S3-LCD-EV-Board](https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32s3/esp32-s3-lcd-ev-board/) with the 4.3" SUB3 subboard (800x480, ST7262 + GT1151) | `esp32s3` | BOOT |
| [ESP32-S31-Korvo-1](https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32s31/esp32-s31-korvo-1/index.html) with its LCD subboard | `esp32s31` | SET |
| [ESP-VoCat](https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32s3/esp-vocat/user_guide_v1.2.html) v1.2 (1.85" 360x360 round LCD, ST77916 + CST816T) | `vocat` | BOOT (inside the case) |

The ESP32-S3-LCD-EV-Board pins are for board versions before v1.5, which have the
ESP32-S3-WROOM-1-N16R8 module.

## Build and flash

You need [espflash](https://github.com/esp-rs/espflash). The pinned nightly toolchain in
`rust-toolchain.toml` builds for the ESP32-S31. The ESP32-S3 is an Xtensa chip and needs
Espressif's toolchain from [espup](https://github.com/esp-rs/espup).

```sh
# ESP32-S31-Korvo-1
cargo run-korvo

# ESP32-S3-LCD-EV-Board
espup install
source ~/export-esp.sh
cargo +esp run-s3

# ESP-VoCat
cargo +esp run-vocat
```

`cargo build-korvo`, `cargo +esp build-s3` and `cargo +esp build-vocat` build without flashing.
If more than one board is connected, add `-- --port <PORT>`.

## Use

- Draw on the white canvas with one finger.
- The toolbar at the top has 8 colors, 3 brush sizes (S, M and L), an eraser and CLEAR.
- The restore button draws the demo picture again.

On the ESP-VoCat, the 8 colors and the eraser (the white dot) are dots along the bottom edge, and
the black circle at the top shows the brush. Tap the front touch pad on the cat's head to clear the
canvas, and the back pad to step through the brush sizes.

Flash two or more boards and draw on any of them. Strokes, CLEAR and the restored picture appear
on all boards on ESP-NOW channel 1. Each board keeps its own color and brush. A board that starts
later sees only new drawing, not the strokes from before it started.

## How it works

- The ESP32-S3's LCD reads the PSRAM framebuffer through a ring of bounce buffers in internal
  RAM. A memory-to-memory DMA refills them, like
  [esp-bsp](https://github.com/espressif/esp-bsp) does for this board. Without them, the CPU's
  PSRAM accesses make the LCD miss pixels, and the picture shifts.
- The ESP32-S31's LCD reads the PSRAM framebuffer directly through a circular DMA descriptor
  chain at 14 MHz, with high DMA priority.
- The ESP-VoCat's ST77916 has its own frame memory. The changed framebuffer rows go to it over
  QSPI at 40 MHz.
- All boards share an 800x480 canvas. The ESP-VoCat shows the 360x360 part of it in the middle
  of the drawing area, below the toolbar of the other boards. Drawing outside of that part does
  not appear on the ESP-VoCat.
- Each board broadcasts its drawing operations with sequence numbers. Each packet repeats the
  last few operations, because ESP-NOW broadcasts have no acknowledgements.

The display settings of the 800x480 boards come from
[mnemosyne-dashboard](https://github.com/bugadani/mnemosyne-dashboard), which uses the same two
boards. The ESP-VoCat's panel initialisation comes from Espressif's ESP-VoCat BSP in
[esp-bsp](https://github.com/espressif/esp-bsp).

## esp-hal version

The ESP32-S31 LCD driver is not in an esp-hal release yet. `Cargo.toml` uses the crates.io
versions and patches them to an esp-hal Git revision. Remove the `[patch.crates-io]` section when
a release has the driver.

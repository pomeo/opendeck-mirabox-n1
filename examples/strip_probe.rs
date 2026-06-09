//! Layout probe for the Mirabox N1 screen strip (indices 15/16/17).
//!
//! Clears, then writes index 15=RED, 16=GREEN, 17=BLUE at a fixed square size and holds it, so we
//! can see how the three strip regions are laid out (side by side / overlapping / gaps).
//!
//!     cargo run --example strip_probe            # size 96 (default)
//!     cargo run --example strip_probe -- 64      # try another size
//!
//! Close OpenDeck first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use image::{DynamicImage, Rgb, RgbImage};
use mirajazz::{
    device::{Device, DeviceQuery, list_devices},
    error::MirajazzError,
    types::{ImageFormat, ImageMirroring, ImageMode, ImageRotation},
};

const QUERY: DeviceQuery = DeviceQuery::new(65440, 1, 0x6603, 0x1000);
const KEY_COUNT: usize = 15;
const ENCODER_COUNT: usize = 3;

static MODE_DONE: AtomicBool = AtomicBool::new(false);

fn format(s: u32) -> ImageFormat {
    ImageFormat {
        mode: ImageMode::JPEG,
        size: (s as usize, s as usize),
        rotation: ImageRotation::Rot0,
        mirror: ImageMirroring::None,
    }
}

/// Solid color with a white top-left marker.
fn img(s: u32, c: [u8; 3]) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(s, s, |x, y| {
        if x < s / 4 && y < s / 4 {
            Rgb([255, 255, 255])
        } else {
            Rgb(c)
        }
    }))
}

async fn session(size: u32) -> Result<(), MirajazzError> {
    let devs = list_devices(&[QUERY]).await?;
    let dev = match devs.into_iter().next() {
        Some(d) => d,
        None => return Ok(()),
    };

    eprintln!("[session] connecting serial={:?}", dev.serial_number);
    let device = Device::connect(&dev, 3, KEY_COUNT, ENCODER_COUNT).await?;
    device.set_brightness(100).await.ok();
    if !MODE_DONE.swap(true, Ordering::SeqCst) {
        eprintln!("set_mode(3) -> {:?}", device.set_mode(3).await);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Sweep shrinking square sizes so we can pick the one that centers each segment over its
    // column. `size` (CLI arg) is the starting size; we step down from there.
    let sizes = [size, size - 8, size - 16, size - 24, size - 32, size - 40];

    loop {
        for s in sizes {
            eprintln!("==> strip 15=RED 16=GREEN 17=BLUE at {s}x{s}");
            device.clear_all_button_images().await.ok();
            device.flush().await.ok();
            device.set_button_image(15, format(s), img(s, [255, 0, 0])).await.ok();
            device.set_button_image(16, format(s), img(s, [0, 255, 0])).await.ok();
            device.set_button_image(17, format(s), img(s, [0, 90, 255])).await.ok();
            if device.flush().await.is_err() {
                return Ok(());
            }
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                device.keep_alive().await.ok();
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let size: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(96);

    eprintln!("Strip layout probe, SIZE={size}. Ctrl-C to stop.");
    loop {
        if let Err(e) = session(size).await {
            eprintln!("[session] error: {e}");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

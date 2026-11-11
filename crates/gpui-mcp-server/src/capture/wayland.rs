//! Screenshots of GPUI windows that are Wayland clients.
//!
//! A Wayland surface has no global window identifier, so the bridge publishes
//! none and the server chooses a capture route from the session it runs in:
//!
//! - Under Hyprland, the compositor's IPC names the windows a process owns and
//!   its toplevel export protocol copies one window's pixels, so the capture is
//!   unattended and bound to the application's process ID.
//! - Elsewhere, the XDG Screenshot portal asks the user to choose what to
//!   capture; the server cannot verify that the choice is the GPUI window.

mod hyprland;
mod portal;

use std::io::Cursor;

use base64::Engine as _;
use gpui_mcp_protocol::{ProcessId, Rect, Screenshot, ScreenshotTarget, WindowGeometry};
use image::{DynamicImage, ImageFormat, RgbaImage};

/// The largest PNG the server reads from, or returns to, a Wayland capture route.
const MAX_PNG_BYTES: usize = 16 * 1024 * 1024;

/// How a Wayland window is captured in this session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Backend {
    /// Hyprland toplevel export, selected through Hyprland IPC by process ID.
    Hyprland,
    /// The interactive XDG Screenshot portal.
    Portal,
}

impl Backend {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Hyprland => "hyprland",
            Self::Portal => "portal",
        }
    }
}

/// The route a capture would take now. Hyprland is chosen only when its IPC
/// socket for this session exists; the portal is the fallback everywhere else.
pub(crate) fn backend() -> Backend {
    if hyprland::available() {
        Backend::Hyprland
    } else {
        Backend::Portal
    }
}

pub(super) async fn screenshot(
    pid: ProcessId,
    area: ScreenshotTarget,
    geometry: WindowGeometry,
) -> Result<Screenshot, String> {
    match backend() {
        Backend::Hyprland => hyprland::screenshot(pid, area, geometry).await,
        Backend::Portal => portal::screenshot(area, Some(geometry)).await,
    }
}

/// Crop a client-area image to the requested region and re-encode it as PNG.
fn finish(
    image: RgbaImage,
    area: ScreenshotTarget,
    geometry: Option<WindowGeometry>,
) -> Result<Screenshot, String> {
    let image = match area {
        ScreenshotTarget::Window => image,
        ScreenshotTarget::Region { rect } => {
            let geometry = geometry
                .filter(|geometry| geometry.is_valid())
                .ok_or_else(|| "GPUI window geometry is unavailable".to_owned())?;
            crop_client_region(&image, rect, geometry)?
        }
    };

    // Re-encode rather than forwarding a source file and its metadata.
    let (width, height) = image.dimensions();
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, ImageFormat::Png)
        .map_err(|_| "could not encode the Wayland screenshot".to_owned())?;
    let encoded = encoded.into_inner();
    if encoded.len() > MAX_PNG_BYTES {
        return Err("encoded Wayland screenshot exceeds the 16 MiB size limit".to_owned());
    }
    Ok(Screenshot {
        mime_type: "image/png".to_owned(),
        base64_data: base64::engine::general_purpose::STANDARD.encode(encoded),
        width,
        height,
    })
}

/// Whether an image has the physical size of GPUI's client area.
fn matches_client_area(image: &RgbaImage, geometry: WindowGeometry) -> bool {
    let scale = f64::from(geometry.scale_factor);
    let expected_width = f64::from(geometry.content_bounds.width) * scale;
    let expected_height = f64::from(geometry.content_bounds.height) * scale;
    expected_width.is_finite()
        && expected_height.is_finite()
        && (expected_width - f64::from(image.width())).abs() <= 2.0
        && (expected_height - f64::from(image.height())).abs() <= 2.0
}

// Wayland screenshots can include compositor-owned decorations and shadows.
// Only crop when the image has the dimensions of GPUI's client area; otherwise
// a guessed offset could return a different part of the desktop.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn crop_client_region(
    image: &RgbaImage,
    rect: Rect,
    geometry: WindowGeometry,
) -> Result<RgbaImage, String> {
    if !matches_client_area(image, geometry) {
        return Err(
            "the captured image includes decorations or does not match the GPUI client area; region capture is unavailable"
                .to_owned(),
        );
    }
    if !rect.is_valid() || rect.width <= 0.0 || rect.height <= 0.0 {
        return Err("screenshot region must be finite and non-empty".to_owned());
    }
    let sx = f64::from(image.width()) / f64::from(geometry.content_bounds.width);
    let sy = f64::from(image.height()) / f64::from(geometry.content_bounds.height);
    let left = (f64::from(rect.x) * sx)
        .floor()
        .clamp(0.0, f64::from(image.width())) as u32;
    let top = (f64::from(rect.y) * sy)
        .floor()
        .clamp(0.0, f64::from(image.height())) as u32;
    let right = (f64::from(rect.x + rect.width) * sx)
        .ceil()
        .clamp(0.0, f64::from(image.width())) as u32;
    let bottom = (f64::from(rect.y + rect.height) * sy)
        .ceil()
        .clamp(0.0, f64::from(image.height())) as u32;
    if right <= left || bottom <= top {
        return Err("screenshot region is outside the application window".to_owned());
    }
    Ok(image::imageops::crop_imm(image, left, top, right - left, bottom - top).to_image())
}

#[cfg(test)]
fn test_geometry(width: f32, height: f32, scale_factor: f32) -> WindowGeometry {
    WindowGeometry {
        content_bounds: Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        },
        scale_factor,
    }
}

#[cfg(test)]
mod tests {
    use image::Rgba;

    use super::*;

    #[test]
    fn regions_are_client_relative() -> Result<(), String> {
        let image = RgbaImage::from_fn(200, 100, |x, _| {
            if x < 100 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        });
        let region = crop_client_region(
            &image,
            Rect {
                x: 100.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            test_geometry(200.0, 100.0, 1.0),
        )?;
        assert_eq!(region.dimensions(), (100, 100));
        assert_eq!(region.get_pixel(0, 0).0, [0, 0, 255, 255]);
        Ok(())
    }

    #[test]
    fn regions_scale_logical_coordinates_to_physical_pixels() -> Result<(), String> {
        let image = RgbaImage::new(400, 200);
        let region = crop_client_region(
            &image,
            Rect {
                x: 10.0,
                y: 10.0,
                width: 50.0,
                height: 20.0,
            },
            test_geometry(200.0, 100.0, 2.0),
        )?;
        assert_eq!(region.dimensions(), (100, 40));
        Ok(())
    }

    #[test]
    fn refuses_ambiguous_window_decorations_and_out_of_bounds_regions() {
        let image = RgbaImage::new(220, 120);
        let rect = Rect {
            x: 20.0,
            y: 0.0,
            width: 40.0,
            height: 40.0,
        };
        assert!(crop_client_region(&image, rect, test_geometry(200.0, 100.0, 1.0)).is_err());
        let image = RgbaImage::new(200, 100);
        let outside = Rect {
            x: 300.0,
            y: 0.0,
            width: 20.0,
            height: 10.0,
        };
        assert!(crop_client_region(&image, outside, test_geometry(200.0, 100.0, 1.0)).is_err());
    }
}

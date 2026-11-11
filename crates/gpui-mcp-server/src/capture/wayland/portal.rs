//! User-mediated Wayland window screenshot capture using XDG Desktop Portal.
//!
//! Version 3 supports requesting window-only capture. Version 2 can provide an
//! interactive window chooser, but cannot enforce its choice: reject images
//! whose dimensions are inconsistent with the instrumented GPUI window. The
//! user must select the correct window; the portal supplies no stable identity.

use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::time::Duration;

use ashpd::desktop::screenshot::{AvailableTargets, ScreenshotOptions, ScreenshotProxy};
use gpui_mcp_protocol::{Screenshot, ScreenshotTarget, WindowGeometry};
use image::{ImageFormat, RgbaImage};
use tokio::time::timeout;

const PORTAL_REQUEST_DEADLINE: Duration = Duration::from_mins(2);
const PORTAL_READ_DEADLINE: Duration = Duration::from_secs(15);
const MAX_PNG_BYTES: usize = super::MAX_PNG_BYTES;
const MAX_IMAGE_DIMENSION: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;

pub(super) async fn screenshot(
    area: ScreenshotTarget,
    geometry: Option<WindowGeometry>,
) -> Result<Screenshot, String> {
    // Always prompt interactively: Wayland cannot map a portal window choice
    // back to GPUI's process ID. On legacy portals the user must choose Window.
    let (uri, legacy) = timeout(PORTAL_REQUEST_DEADLINE, async {
        let proxy = ScreenshotProxy::new()
            .await
            .map_err(|_| "Wayland Screenshot portal is unavailable".to_owned())?;
        if proxy.version() < 2 {
            return Err(
                "Wayland screenshots require an interactive XDG Screenshot portal".to_owned(),
            );
        }
        let legacy = proxy.version() < 3;
        let mut options = ScreenshotOptions::default()
            .set_interactive(true)
            .set_modal(false);
        if !legacy {
            let targets = proxy
                .available_targets()
                .await
                .map_err(|_| "could not query Wayland screenshot targets".to_owned())?;
            if !targets.contains(AvailableTargets::Window) {
                return Err("the Wayland portal does not support user-selected windows".to_owned());
            }
            options = options.set_target(AvailableTargets::Window);
        }
        let request = proxy
            .screenshot(None, options)
            .await
            .map_err(|_| "could not request a Wayland window screenshot".to_owned())?;
        let response = request
            .response()
            .map_err(|_| "Wayland window selection was cancelled or denied".to_owned())?;
        Ok::<_, String>((response.uri().as_str().to_owned(), legacy))
    })
    .await
    .map_err(|_| "Wayland window selection exceeded the 120 second deadline".to_owned())??;

    let url = url::Url::parse(&uri)
        .map_err(|_| "the screenshot portal returned an invalid URI".to_owned())?;
    let path = url
        .to_file_path()
        .map_err(|()| "the screenshot portal returned a non-file URI".to_owned())?;

    timeout(
        PORTAL_READ_DEADLINE,
        tokio::task::spawn_blocking(move || capture_saved_png(&path, area, geometry, legacy)),
    )
    .await
    .map_err(|_| "reading the Wayland screenshot exceeded the 15 second deadline".to_owned())?
    .map_err(|_| "Wayland screenshot decoder failed".to_owned())?
}

fn capture_saved_png(
    path: &Path,
    area: ScreenshotTarget,
    geometry: Option<WindowGeometry>,
    legacy: bool,
) -> Result<Screenshot, String> {
    // The URI comes from the session portal, not from a tool argument. Still
    // bound the file read, reject symlinks, and check ownership. Never follow
    // an attacker-controlled symlink to a file outside the screenshot portal.
    let source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "could not safely open the portal screenshot".to_owned())?;
    let metadata = source
        .metadata()
        .map_err(|_| "could not inspect the portal screenshot".to_owned())?;
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME must be set to validate portal screenshot ownership".to_owned())?;
    let owner_uid = std::fs::metadata(home)
        .map_err(|_| "could not determine screenshot owner".to_owned())?
        .uid();
    if !metadata.is_file() || metadata.uid() != owner_uid || metadata.len() > MAX_PNG_BYTES as u64 {
        return Err("portal screenshot is not a regular, user-owned file under 16 MiB".to_owned());
    }
    let mut bytes = Vec::new();
    source
        .take(MAX_PNG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read the portal screenshot".to_owned())?;
    decode_png(&bytes, area, geometry, legacy)
}

fn decode_png(
    bytes: &[u8],
    area: ScreenshotTarget,
    geometry: Option<WindowGeometry>,
    legacy: bool,
) -> Result<Screenshot, String> {
    if bytes.len() > MAX_PNG_BYTES {
        return Err("portal screenshot exceeds the 16 MiB size limit".to_owned());
    }
    // PNG IHDR dimensions can be validated before allocating a pixel buffer.
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return Err("portal screenshot is not a PNG image".to_owned());
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err("portal screenshot dimensions exceed the safety bound".to_owned());
    }

    let image = image::load_from_memory_with_format(bytes, ImageFormat::Png)
        .map_err(|_| "could not decode the portal screenshot".to_owned())?
        .to_rgba8();
    if legacy {
        let geometry = geometry
            .filter(|geometry| geometry.is_valid())
            .ok_or_else(|| "GPUI window geometry is unavailable".to_owned())?;
        verify_legacy_window_dimensions(&image, geometry)?;
    }
    super::finish(image, area, geometry)
}

// A legacy v2 portal may return a screen or an arbitrary user-selected area.
// Reject outputs whose dimensions clearly cannot be the GPUI window. This is
// a safety check, not an identity proof; the user still selects the window.
fn verify_legacy_window_dimensions(
    image: &RgbaImage,
    geometry: WindowGeometry,
) -> Result<(), String> {
    let scale = f64::from(geometry.scale_factor);
    let expected_width = f64::from(geometry.content_bounds.width) * scale;
    let expected_height = f64::from(geometry.content_bounds.height) * scale;
    let margin = 96.0 * scale;
    if !expected_width.is_finite()
        || !expected_height.is_finite()
        || (f64::from(image.width()) - expected_width).abs() > margin
        || (f64::from(image.height()) - expected_height).abs() > margin
    {
        return Err(
            "legacy Wayland portal screenshot does not match the GPUI window size; choose Window in the screenshot dialog"
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use gpui_mcp_protocol::Rect;
    use image::DynamicImage;
    use image::Rgba;
    use std::io::Cursor;

    fn geometry(width: f32, height: f32) -> WindowGeometry {
        WindowGeometry {
            content_bounds: Rect {
                x: 0.0,
                y: 0.0,
                width,
                height,
            },
            scale_factor: 1.0,
        }
    }

    #[test]
    fn legacy_portal_rejects_monitor_sized_images_for_small_windows() {
        let image = RgbaImage::new(1920, 1080);
        assert!(verify_legacy_window_dimensions(&image, geometry(800.0, 600.0)).is_err());
        let image = RgbaImage::new(810, 640);
        assert!(verify_legacy_window_dimensions(&image, geometry(800.0, 600.0)).is_ok());
    }

    fn png(image: &RgbaImage) -> Result<Vec<u8>, String> {
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut encoded, ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        Ok(encoded.into_inner())
    }

    #[test]
    fn a_portal_png_is_reencoded_and_region_cropped() -> Result<(), String> {
        let image = RgbaImage::from_fn(200, 100, |x, _| {
            if x < 100 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        });
        let bytes = png(&image)?;
        let window = decode_png(&bytes, ScreenshotTarget::Window, None, false)?;
        assert_eq!((window.width, window.height), (200, 100));
        assert_eq!(window.mime_type, "image/png");

        let region = ScreenshotTarget::Region {
            rect: Rect {
                x: 100.0,
                y: 0.0,
                width: 50.0,
                height: 50.0,
            },
        };
        let crop = decode_png(&bytes, region, Some(geometry(200.0, 100.0)), true)?;
        assert_eq!((crop.width, crop.height), (50, 50));
        let decoded = image::load_from_memory(
            &base64::engine::general_purpose::STANDARD
                .decode(crop.base64_data)
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
        .to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 255, 255]);
        Ok(())
    }

    #[test]
    fn a_legacy_portal_image_needs_window_geometry() -> Result<(), String> {
        let bytes = png(&RgbaImage::new(200, 100))?;
        assert!(decode_png(&bytes, ScreenshotTarget::Window, None, true).is_err());
        Ok(())
    }

    #[test]
    fn portal_files_must_be_regular_png_files_reached_without_symlinks() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let real = directory.path().join("shot.png");
        std::fs::write(&real, png(&RgbaImage::new(20, 10))?).map_err(|error| error.to_string())?;
        let shot = capture_saved_png(&real, ScreenshotTarget::Window, None, false)?;
        assert_eq!((shot.width, shot.height), (20, 10));

        let link = directory.path().join("link.png");
        std::os::unix::fs::symlink(&real, &link).map_err(|error| error.to_string())?;
        assert!(capture_saved_png(&link, ScreenshotTarget::Window, None, false).is_err());

        let text = directory.path().join("text.png");
        std::fs::write(&text, b"not an image").map_err(|error| error.to_string())?;
        assert!(capture_saved_png(&text, ScreenshotTarget::Window, None, false).is_err());

        assert!(
            capture_saved_png(directory.path(), ScreenshotTarget::Window, None, false).is_err()
        );
        Ok(())
    }

    #[test]
    fn rejects_png_header_with_excessive_dimensions() {
        let mut png = Vec::from(&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..]);
        png.extend_from_slice(&u32::MAX.to_be_bytes());
        png.extend_from_slice(&100_u32.to_be_bytes());
        assert!(decode_png(&png, ScreenshotTarget::Window, None, false).is_err());
    }
}

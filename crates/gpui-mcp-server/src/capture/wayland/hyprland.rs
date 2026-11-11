//! Unattended capture of one Hyprland window, bound to the application's PID.
//!
//! Hyprland's IPC socket lists every window with the process ID the compositor
//! read from the client's Wayland connection, so the server can find the GPUI
//! window without asking the user. The `hyprland_toplevel_export_v1` protocol
//! then copies that window's surface, without server-side decorations, into a
//! shared-memory buffer.
//!
//! Hyprland offers both interfaces to every client of the session; this is the
//! same trust model as X11 capture, not the per-request consent of the portal.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::fs::FileExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_mcp_protocol::{ProcessId, Screenshot, ScreenshotTarget, WindowGeometry};
use image::RgbaImage;
use serde::Deserialize;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};

use self::protocol::hyprland_toplevel_export_frame_v1::{
    self as frame_v1, HyprlandToplevelExportFrameV1,
};
use self::protocol::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;

/// Bound on the compositor's reply to one capture, from connection to pixels.
const CAPTURE_DEADLINE: Duration = Duration::from_secs(10);
/// How long to wait for a window's frame to reach the GPUI client area's size.
const SETTLE_PERIOD: Duration = Duration::from_secs(2);
/// Pause between capture attempts while a window settles.
const SETTLE_INTERVAL: Duration = Duration::from_millis(50);
/// Bound on one Hyprland IPC request.
const IPC_DEADLINE: Duration = Duration::from_secs(2);
/// Bound on the Hyprland IPC client list; real lists are a few kilobytes.
const MAX_IPC_BYTES: u64 = 8 * 1024 * 1024;
/// Largest window edge accepted from the compositor, in physical pixels.
const MAX_DIMENSION: u32 = 16_384;
/// Largest window area accepted from the compositor, in physical pixels.
const MAX_PIXELS: u64 = 40 * 1024 * 1024;

#[allow(
    missing_docs,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    non_upper_case_globals,
    non_camel_case_types
)]
mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}

/// Whether this process runs inside a Hyprland session with a reachable IPC socket.
pub(super) fn available() -> bool {
    socket_path().is_some_and(|path| path.exists())
}

/// Hyprland's request socket: `$XDG_RUNTIME_DIR/hypr/<signature>/.socket.sock`.
fn socket_path() -> Option<PathBuf> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let signature = Path::new(&signature);
    if !is_single_directory_name(signature) {
        return None;
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(
        PathBuf::from(runtime)
            .join("hypr")
            .join(signature)
            .join(".socket.sock"),
    )
}

/// The signature names one directory; refuse anything that could leave it.
fn is_single_directory_name(name: &Path) -> bool {
    let mut components = name.components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

pub(super) async fn screenshot(
    pid: ProcessId,
    area: ScreenshotTarget,
    geometry: WindowGeometry,
) -> Result<Screenshot, String> {
    let path = socket_path().ok_or_else(|| "the Hyprland IPC socket is unavailable".to_owned())?;
    tokio::time::timeout(
        CAPTURE_DEADLINE + IPC_DEADLINE,
        tokio::task::spawn_blocking(move || {
            let handle = select_window(&clients(&path)?, pid.get(), geometry)?;
            let image = capture_toplevel(handle, geometry)?;
            super::finish(image, area, Some(geometry))
        }),
    )
    .await
    .map_err(|_| "Hyprland window capture exceeded its deadline".to_owned())?
    .map_err(|_| "Hyprland window capture worker failed".to_owned())?
}

/// One window as `hyprctl clients -j` reports it.
#[derive(Debug, Deserialize)]
struct Client {
    /// Window address in hexadecimal, such as `0x55d0d161e7b0`.
    address: String,
    /// Owning process, as the compositor read it from the client connection.
    pid: i64,
    mapped: bool,
    hidden: bool,
    /// Logical size of the window's client area.
    size: [i32; 2],
}

fn clients(path: &Path) -> Result<Vec<Client>, String> {
    let mut stream = UnixStream::connect(path)
        .map_err(|_| "could not connect to the Hyprland IPC socket".to_owned())?;
    stream
        .set_read_timeout(Some(IPC_DEADLINE))
        .and_then(|()| stream.set_write_timeout(Some(IPC_DEADLINE)))
        .map_err(|_| "could not bound the Hyprland IPC request".to_owned())?;
    stream
        .write_all(b"j/clients")
        .map_err(|_| "could not send the Hyprland IPC request".to_owned())?;
    let mut reply = Vec::new();
    stream
        .take(MAX_IPC_BYTES + 1)
        .read_to_end(&mut reply)
        .map_err(|_| "could not read the Hyprland IPC reply".to_owned())?;
    if reply.len() as u64 > MAX_IPC_BYTES {
        return Err("the Hyprland window list exceeds its size bound".to_owned());
    }
    serde_json::from_slice(&reply)
        .map_err(|_| "Hyprland returned an unreadable window list".to_owned())
}

/// Choose the GPUI window among the windows Hyprland attributes to its process.
///
/// One mapped, visible window is the answer. Several are narrowed to those with
/// the GPUI client area's logical size; if that still leaves more than one, the
/// capture fails rather than guessing which window the bridge describes.
fn select_window(windows: &[Client], pid: u32, geometry: WindowGeometry) -> Result<u32, String> {
    let owned: Vec<&Client> = windows
        .iter()
        .filter(|window| window.pid == i64::from(pid) && window.mapped && !window.hidden)
        .collect();
    let candidates: Vec<&Client> = if owned.len() > 1 {
        owned
            .into_iter()
            .filter(|window| {
                (f64::from(window.size[0]) - f64::from(geometry.content_bounds.width)).abs() <= 1.0
                    && (f64::from(window.size[1]) - f64::from(geometry.content_bounds.height)).abs()
                        <= 1.0
            })
            .collect()
    } else {
        owned
    };
    match candidates.as_slice() {
        [] => Err("Hyprland lists no visible window for the application's process".to_owned()),
        [window] => handle(&window.address),
        _ => Err(
            "the application has several Hyprland windows of the same size; the GPUI window is ambiguous"
                .to_owned(),
        ),
    }
}

/// The protocol's handle is the low 32 bits of the window address.
#[allow(clippy::cast_possible_truncation)]
fn handle(address: &str) -> Result<u32, String> {
    let digits = address
        .strip_prefix("0x")
        .ok_or_else(|| "Hyprland returned a malformed window address".to_owned())?;
    let address = u64::from_str_radix(digits, 16)
        .map_err(|_| "Hyprland returned a malformed window address".to_owned())?;
    Ok((address & 0xFFFF_FFFF) as u32)
}

/// The shared-memory layout the compositor asked for.
#[derive(Clone, Copy, Debug)]
struct ShmLayout {
    format: WEnum<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Default)]
struct State {
    layout: Option<ShmLayout>,
    buffer_done: bool,
    y_invert: bool,
    /// `Some(true)` after `ready`, `Some(false)` after `failed`.
    outcome: Option<bool>,
}

/// Capture the window once its frame has the GPUI client area's size.
///
/// While Hyprland animates a window opening or resizing, the exported surface
/// has an intermediate size, and a resize between the buffer offer and the copy
/// makes the compositor refuse the copy. Retry on a fresh frame until one has
/// the expected size or the settle period ends; after that, a window capture
/// returns the last frame and a region capture reports the mismatch.
fn capture_toplevel(handle: u32, geometry: WindowGeometry) -> Result<RgbaImage, String> {
    let deadline = Instant::now() + CAPTURE_DEADLINE;
    let settle = Instant::now() + SETTLE_PERIOD;
    let connection = Connection::connect_to_env()
        .map_err(|_| "could not connect to the Wayland compositor".to_owned())?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)
        .map_err(|_| "could not list Wayland globals".to_owned())?;
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| "the compositor offers no shared-memory buffers".to_owned())?;
    let manager: HyprlandToplevelExportManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| "this Hyprland does not offer toplevel export".to_owned())?;

    let result = loop {
        let mut state = State::default();
        let frame = manager.capture_toplevel(0, handle, &qh, ());
        let attempt = dispatch_until(&mut queue, &mut state, deadline, |state| {
            state.buffer_done || state.outcome.is_some()
        })
        .and_then(|()| copy_frame(&frame, &shm, &qh, &mut queue, &mut state, deadline));
        frame.destroy();
        let settled = matches!(&attempt, Ok(image) if super::matches_client_area(image, geometry));
        if settled || Instant::now() >= settle {
            break attempt;
        }
        std::thread::sleep(SETTLE_INTERVAL);
    };
    manager.destroy();
    let _ = connection.flush();
    result
}

fn copy_frame(
    frame: &HyprlandToplevelExportFrameV1,
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
) -> Result<RgbaImage, String> {
    if state.outcome == Some(false) {
        return Err("Hyprland could not capture the application window".to_owned());
    }
    let layout = state
        .layout
        .ok_or_else(|| "Hyprland offered no shared-memory buffer for the window".to_owned())?;
    let WEnum::Value(format) = layout.format else {
        return Err("Hyprland offered an unknown pixel format".to_owned());
    };
    let size = validate_layout(layout)?;
    let pool_size = i32::try_from(size).map_err(|_| "the window buffer is too large".to_owned())?;
    let (width, height, stride) = (
        i32::try_from(layout.width).map_err(|_| "the window is too wide".to_owned())?,
        i32::try_from(layout.height).map_err(|_| "the window is too tall".to_owned())?,
        i32::try_from(layout.stride).map_err(|_| "the window stride is too large".to_owned())?,
    );

    let memory = File::from(
        rustix::fs::memfd_create("gpui-mcp-capture", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|_| "could not allocate the capture buffer".to_owned())?,
    );
    memory
        .set_len(size)
        .map_err(|_| "could not size the capture buffer".to_owned())?;
    let pool = shm.create_pool(memory.as_fd(), pool_size, qh, ());
    let buffer = pool.create_buffer(0, width, height, stride, format, qh, ());
    frame.copy(&buffer, 1);
    let copied = dispatch_until(queue, state, deadline, |state| state.outcome.is_some());
    buffer.destroy();
    pool.destroy();
    copied?;
    if state.outcome != Some(true) {
        return Err("Hyprland could not copy the application window".to_owned());
    }

    let length = usize::try_from(size).map_err(|_| "the window buffer is too large".to_owned())?;
    let mut bytes = vec![0; length];
    memory
        .read_exact_at(&mut bytes, 0)
        .map_err(|_| "could not read the capture buffer".to_owned())?;
    to_rgba(format, layout, &bytes, state.y_invert)
}

/// Check the compositor's buffer layout and return its size in bytes.
fn validate_layout(layout: ShmLayout) -> Result<u64, String> {
    let (width, height, stride) = (
        u64::from(layout.width),
        u64::from(layout.height),
        u64::from(layout.stride),
    );
    if layout.width == 0
        || layout.height == 0
        || layout.width > MAX_DIMENSION
        || layout.height > MAX_DIMENSION
        || width * height > MAX_PIXELS
        || stride < width * 4
        || stride > width * 4 + 4096
    {
        return Err("Hyprland offered a window buffer outside the safety bound".to_owned());
    }
    Ok(stride * height)
}

/// Convert a 32-bit shared-memory frame to RGBA, honoring row inversion.
fn to_rgba(
    format: wl_shm::Format,
    layout: ShmLayout,
    bytes: &[u8],
    y_invert: bool,
) -> Result<RgbaImage, String> {
    // wl_shm formats name channels from the most significant byte of a
    // little-endian word, so ARGB8888 is stored as B, G, R, A.
    let (bgr, opaque) = match format {
        wl_shm::Format::Argb8888 => (true, false),
        wl_shm::Format::Xrgb8888 => (true, true),
        wl_shm::Format::Abgr8888 => (false, false),
        wl_shm::Format::Xbgr8888 => (false, true),
        _ => {
            return Err(format!(
                "Hyprland offered unsupported pixel format {format:?}"
            ));
        }
    };
    let stride = usize::try_from(layout.stride).map_err(|_| "stride overflow".to_owned())?;
    let row_bytes = usize::try_from(layout.width).map_err(|_| "width overflow".to_owned())? * 4;
    let mut image = RgbaImage::new(layout.width, layout.height);
    for (y, row) in image.rows_mut().enumerate() {
        let source_row = if y_invert {
            usize::try_from(layout.height).map_err(|_| "height overflow".to_owned())? - 1 - y
        } else {
            y
        };
        let start = source_row * stride;
        let source = bytes
            .get(start..start + row_bytes)
            .ok_or_else(|| "the capture buffer is shorter than its layout".to_owned())?;
        for (pixel, channels) in row.zip(source.chunks_exact(4)) {
            let [first, second, third, alpha] =
                [channels[0], channels[1], channels[2], channels[3]];
            let (red, blue) = if bgr { (third, first) } else { (first, third) };
            pixel.0 = [red, second, blue, if opaque { 255 } else { alpha }];
        }
    }
    Ok(image)
}

/// Dispatch compositor events until `done` holds or the deadline passes.
fn dispatch_until(
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Instant,
    done: impl Fn(&State) -> bool,
) -> Result<(), String> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    loop {
        queue
            .dispatch_pending(state)
            .map_err(|_| "the Wayland connection failed".to_owned())?;
        if done(state) {
            return Ok(());
        }
        queue
            .flush()
            .map_err(|_| "could not send the Wayland capture request".to_owned())?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Hyprland did not deliver the window frame in time".to_owned());
        }
        let timeout = Timespec {
            tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let readable = {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            match poll(&mut fds, Some(&timeout)) {
                Ok(_) => !fds[0].revents().is_empty(),
                Err(rustix::io::Errno::INTR) => false,
                Err(_) => return Err("could not wait for the compositor".to_owned()),
            }
        };
        if readable {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err("the Wayland connection failed".to_owned()),
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<HyprlandToplevelExportManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &HyprlandToplevelExportManagerV1,
        _: <HyprlandToplevelExportManagerV1 as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<HyprlandToplevelExportFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &HyprlandToplevelExportFrameV1,
        event: frame_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                state.layout.get_or_insert(ShmLayout {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            frame_v1::Event::BufferDone => state.buffer_done = true,
            frame_v1::Event::Flags { flags } => {
                state.y_invert = matches!(
                    flags,
                    WEnum::Value(flags) if flags.contains(frame_v1::Flags::YInvert)
                );
            }
            frame_v1::Event::Ready { .. } => state.outcome = Some(true),
            frame_v1::Event::Failed => state.outcome = Some(false),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_geometry;
    use super::*;

    fn client(address: &str, pid: i64, size: [i32; 2]) -> Client {
        Client {
            address: address.to_owned(),
            pid,
            mapped: true,
            hidden: false,
            size,
        }
    }

    #[test]
    fn the_handle_is_the_low_half_of_the_window_address() -> Result<(), String> {
        // The protocol's own example: window d161e7b0 has handle 3512854448.
        assert_eq!(handle("0xd161e7b0")?, 3_512_854_448);
        assert_eq!(handle("0x55d0d161e7b0")?, 3_512_854_448);
        assert!(handle("d161e7b0").is_err());
        assert!(handle("0xnothex").is_err());
        Ok(())
    }

    #[test]
    fn the_only_visible_window_of_the_process_is_selected() -> Result<(), String> {
        let mut hidden = client("0x30", 42, [800, 600]);
        hidden.hidden = true;
        let windows = [
            client("0x10", 7, [800, 600]),
            client("0x20", 42, [1024, 768]),
            hidden,
        ];
        assert_eq!(
            select_window(&windows, 42, test_geometry(800.0, 600.0, 1.0))?,
            0x20
        );
        assert!(select_window(&windows, 99, test_geometry(800.0, 600.0, 1.0)).is_err());
        Ok(())
    }

    #[test]
    fn several_windows_are_told_apart_by_client_size_or_refused() -> Result<(), String> {
        let windows = [
            client("0x10", 42, [800, 600]),
            client("0x20", 42, [400, 300]),
        ];
        assert_eq!(
            select_window(&windows, 42, test_geometry(400.0, 300.0, 2.0))?,
            0x20
        );
        let twins = [
            client("0x10", 42, [800, 600]),
            client("0x20", 42, [800, 600]),
        ];
        assert!(select_window(&twins, 42, test_geometry(800.0, 600.0, 1.0)).is_err());
        Ok(())
    }

    #[test]
    fn hyprland_window_lists_parse() -> Result<(), String> {
        let reply = br#"[{"address":"0x5a1e","mapped":true,"hidden":false,"at":[0,0],
            "size":[640,480],"workspace":{"id":1,"name":"1"},"pid":1234,"title":"t"}]"#;
        let windows: Vec<Client> = serde_json::from_slice(reply).map_err(|e| e.to_string())?;
        assert_eq!(windows[0].pid, 1234);
        assert_eq!(windows[0].size, [640, 480]);
        Ok(())
    }

    fn layout(width: u32, height: u32, stride: u32) -> ShmLayout {
        ShmLayout {
            format: WEnum::Value(wl_shm::Format::Argb8888),
            width,
            height,
            stride,
        }
    }

    #[test]
    fn shared_memory_formats_convert_to_rgba() -> Result<(), String> {
        // Two rows of one pixel each, padded to an 8-byte stride.
        let bytes = [
            10, 20, 30, 40, 0, 0, 0, 0, // row 0: B G R A
            50, 60, 70, 80, 0, 0, 0, 0, // row 1
        ];
        let argb = to_rgba(wl_shm::Format::Argb8888, layout(1, 2, 8), &bytes, false)?;
        assert_eq!(argb.get_pixel(0, 0).0, [30, 20, 10, 40]);
        let xrgb = to_rgba(wl_shm::Format::Xrgb8888, layout(1, 2, 8), &bytes, false)?;
        assert_eq!(xrgb.get_pixel(0, 0).0, [30, 20, 10, 255]);
        let abgr = to_rgba(wl_shm::Format::Abgr8888, layout(1, 2, 8), &bytes, false)?;
        assert_eq!(abgr.get_pixel(0, 0).0, [10, 20, 30, 40]);
        let inverted = to_rgba(wl_shm::Format::Argb8888, layout(1, 2, 8), &bytes, true)?;
        assert_eq!(inverted.get_pixel(0, 0).0, [70, 60, 50, 80]);
        assert!(to_rgba(wl_shm::Format::Rgb565, layout(1, 2, 8), &bytes, false).is_err());
        assert!(to_rgba(wl_shm::Format::Argb8888, layout(1, 3, 8), &bytes, false).is_err());
        Ok(())
    }

    #[test]
    fn buffer_layouts_are_bounded() {
        assert_eq!(validate_layout(layout(10, 10, 40)), Ok(400));
        assert!(validate_layout(layout(10, 10, 39)).is_err());
        assert!(validate_layout(layout(0, 10, 40)).is_err());
        assert!(validate_layout(layout(MAX_DIMENSION + 1, 1, (MAX_DIMENSION + 1) * 4)).is_err());
        assert!(validate_layout(layout(10, 10, 40 + 8192)).is_err());
    }

    #[test]
    fn instance_signatures_cannot_leave_the_runtime_directory() {
        for bad in ["..", "a/b", "/abs", "", "."] {
            assert!(
                !is_single_directory_name(Path::new(bad)),
                "{bad:?} was accepted"
            );
        }
        assert!(is_single_directory_name(Path::new(
            "4c27ba9_1762890000_123456789"
        )));
    }
}

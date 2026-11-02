//! Short-lived, input-transparent layer-shell labels; no terminal focus changes.
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    io::Write,
    os::fd::AsFd,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

struct Label {
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    _file: tempfile::NamedTempFile,
    layer: ZwlrLayerSurfaceV1,
}
#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<ZwlrLayerShellV1>,
    outputs: HashMap<u32, (wl_output::WlOutput, String)>,
    labels: HashMap<u32, Label>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_output" if version >= 4 => {
                    state
                        .outputs
                        .insert(name, (registry.bind(name, 4, qh, name), String::new()));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(label) = state.labels.remove(&name) {
                    label.layer.destroy();
                    label.surface.destroy();
                    label.buffer.destroy();
                }
                if let Some((output, _)) = state.outputs.remove(&name) {
                    output.release();
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(output) = state.outputs.get_mut(id)
        {
            output.1 = name;
        }
    }
}
impl Dispatch<ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer.ack_configure(serial);
                if let Some(label) = state.labels.get(id) {
                    label.surface.attach(Some(&label.buffer), 0, 0);
                    label.surface.damage_buffer(0, 0, 256, 160);
                    label.surface.commit();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(label) = state.labels.remove(id) {
                    label.layer.destroy();
                    label.surface.destroy();
                    label.buffer.destroy();
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format {
            format: WEnum::Value(_),
        } = event
        {}
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wayland_client::protocol::wl_region::WlRegion);
delegate_noop!(State: ignore ZwlrLayerShellV1);

// Cairo uses the system font backend and renders native-endian ARGB32, matching
// Wayland's buffer format. Keep the small C interface local to the renderer.
#[repr(C)]
#[derive(Default)]
struct TextExtents {
    x_bearing: f64,
    y_bearing: f64,
    width: f64,
    height: f64,
    x_advance: f64,
    y_advance: f64,
}
type Cairo = std::ffi::c_void;
#[link(name = "cairo")]
unsafe extern "C" {
    fn cairo_image_surface_create_for_data(
        data: *mut u8,
        format: i32,
        width: i32,
        height: i32,
        stride: i32,
    ) -> *mut Cairo;
    fn cairo_surface_status(surface: *mut Cairo) -> i32;
    fn cairo_surface_flush(surface: *mut Cairo);
    fn cairo_surface_destroy(surface: *mut Cairo);
    fn cairo_create(surface: *mut Cairo) -> *mut Cairo;
    fn cairo_destroy(context: *mut Cairo);
    fn cairo_status(context: *mut Cairo) -> i32;
    fn cairo_select_font_face(
        context: *mut Cairo,
        family: *const std::ffi::c_char,
        slant: i32,
        weight: i32,
    );
    fn cairo_set_font_size(context: *mut Cairo, size: f64);
    fn cairo_text_extents(
        context: *mut Cairo,
        text: *const std::ffi::c_char,
        extents: *mut TextExtents,
    );
    fn cairo_set_source_rgb(context: *mut Cairo, red: f64, green: f64, blue: f64);
    fn cairo_move_to(context: *mut Cairo, x: f64, y: f64);
    fn cairo_show_text(context: *mut Cairo, text: *const std::ffi::c_char);
}

fn pixels(number: usize, border: (u32, usize)) -> Result<Vec<u8>> {
    let mut pixels = vec![0u32; 256 * 160];
    for y in 0..160 {
        for x in 0..256 {
            pixels[y * 256 + x] = if !(border.1..256 - border.1).contains(&x)
                || !(border.1..160 - border.1).contains(&y)
            {
                border.0
            } else {
                crate::theme::OVERLAY_BACKGROUND
            };
        }
    }
    let text = std::ffi::CString::new(number.to_string())?;
    let mut extents = TextExtents::default();
    // The u32 allocation is aligned and remains alive and unmoved until Cairo
    // releases both objects. Cairo returns valid error objects on failure.
    let status = unsafe {
        let surface =
            cairo_image_surface_create_for_data(pixels.as_mut_ptr().cast(), 0, 256, 160, 256 * 4);
        let context = cairo_create(surface);
        cairo_select_font_face(context, crate::theme::OVERLAY_FONT.as_ptr(), 0, 0);
        cairo_set_font_size(context, 128.);
        cairo_text_extents(context, text.as_ptr(), &mut extents);
        let fit = (220. / extents.width.max(1.))
            .min(112. / extents.height.max(1.))
            .min(1.);
        cairo_set_font_size(context, 128. * fit);
        cairo_text_extents(context, text.as_ptr(), &mut extents);
        let color = crate::theme::OVERLAY_TEXT;
        cairo_set_source_rgb(
            context,
            f64::from((color >> 16) & 255) / 255.,
            f64::from((color >> 8) & 255) / 255.,
            f64::from(color & 255) / 255.,
        );
        cairo_move_to(
            context,
            (256. - extents.width) / 2. - extents.x_bearing,
            (160. - extents.height) / 2. - extents.y_bearing,
        );
        cairo_show_text(context, text.as_ptr());
        cairo_surface_flush(surface);
        let status = (cairo_status(context), cairo_surface_status(surface));
        cairo_destroy(context);
        cairo_surface_destroy(surface);
        status
    };
    ensure!(
        status == (0, 0),
        "Cannot render identification font: Cairo status {status:?}"
    );
    Ok(pixels.into_iter().flat_map(u32::to_ne_bytes).collect())
}

pub fn show(numbers: HashMap<String, usize>) -> Result<()> {
    let border = crate::theme::overlay_border()?;
    ensure!(
        numbers.values().all(|n| *n < 1000),
        "Too many identification labels"
    );
    let connection = Connection::connect_to_env().context("No Wayland display for overlays")?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let compositor = state.compositor.clone().context("Compositor unavailable")?;
    let shm = state
        .shm
        .clone()
        .context("Shared-memory protocol unavailable")?;
    let shell = state.shell.clone().context("Layer-shell unsupported")?;
    for (id, (output, name)) in &state.outputs {
        let Some(number) = numbers.get(name) else {
            continue;
        };
        let surface = compositor.create_surface(&qh, ());
        let region = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let layer = shell.get_layer_surface(
            &surface,
            Some(output),
            zwlr_layer_shell_v1::Layer::Overlay,
            "hypr-monitor-tui-identify".into(),
            &qh,
            *id,
        );
        layer.set_size(256, 160);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(&pixels(*number, border)?)?;
        let pool = shm.create_pool(file.as_fd(), 256 * 160 * 4, &qh, ());
        let buffer = pool.create_buffer(0, 256, 160, 256 * 4, wl_shm::Format::Argb8888, &qh, ());
        pool.destroy();
        surface.commit();
        state.labels.insert(
            *id,
            Label {
                surface,
                buffer,
                _file: file,
                layer,
            },
        );
    }
    ensure!(
        !state.labels.is_empty(),
        "No active named Wayland outputs for identification"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !state.labels.is_empty() {
        queue.dispatch_pending(&mut state)?;
        connection.flush()?;
        if let Some(guard) = queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&connection.backend().poll_fd()),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut fd, 1, 100) };
            if result > 0 {
                guard.read()?;
            } else if result < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_labels_fit_and_include_antialiased_pixels() {
        for number in [1, 10, 999] {
            let border = (0xff033e8c, 2);
            let bytes = pixels(number, border).unwrap();
            assert_eq!(bytes.len(), 256 * 160 * 4);
            let colors: Vec<_> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| u32::from_ne_bytes(*p))
                .collect();
            assert!(colors.contains(&crate::theme::OVERLAY_TEXT));
            assert!(colors.iter().any(|c| {
                ![
                    crate::theme::OVERLAY_TEXT,
                    border.0,
                    crate::theme::OVERLAY_BACKGROUND,
                ]
                .contains(c)
            }));
            for y in 0..160 {
                for x in 0..256 {
                    if !(2..254).contains(&x) || !(2..158).contains(&y) {
                        assert_eq!(colors[y * 256 + x], border.0);
                    }
                }
            }
        }
    }
}

// language: Rust, file: src/ui/metal_renderer.rs, target: macOS (Metal) & Linux (Wayland/X11)
use crate::{codec::decoder::DecodedFrame, input::coordinate_mapper::DisplayRotation};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(target_os = "macos")]
use crate::codec::videotoolbox_sys::*;
#[cfg(target_os = "macos")]
use core_graphics_types::geometry::CGSize;
#[cfg(target_os = "macos")]
use foreign_types::ForeignType;
#[cfg(target_os = "macos")]
use metal::{
    CommandQueue, Device, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLStoreAction,
    MTLViewport, MetalLayer, RenderPassDescriptor, RenderPipelineDescriptor, RenderPipelineState,
    TextureRef,
};
#[cfg(target_os = "macos")]
use std::{ffi::c_void, ptr};

pub struct MetalRenderer {
    #[cfg(target_os = "macos")]
    command_queue: CommandQueue,
    #[cfg(target_os = "macos")]
    layer: MetalLayer,
    #[cfg(target_os = "macos")]
    pipeline: RenderPipelineState,
    #[cfg(target_os = "macos")]
    texture_cache: CVMetalTextureCacheRef,
    #[cfg(not(target_os = "macos"))]
    linux: std::sync::Mutex<LinuxRendererState>,
    rotation: AtomicU32,
}

impl MetalRenderer {
    #[cfg(target_os = "macos")]
    #[allow(unexpected_cfgs)]
    pub fn new(window: &winit::window::Window) -> Result<Self> {
        use objc::{msg_send, runtime::YES, sel, sel_impl};
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let device = Device::system_default().context("No Metal-compatible GPU found")?;
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(false);
        layer.set_display_sync_enabled(false);

        let handle = window
            .window_handle()
            .context("Could not get AppKit window handle")?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            bail!("Expected an AppKit window handle");
        };
        unsafe {
            let _: () = msg_send![layer.as_ref(), setAllowsNextDrawableTimeout: YES];
            let view = handle.ns_view.as_ptr() as *mut objc::runtime::Object;
            let _: () = msg_send![view, setWantsLayer: YES];
            let _: () = msg_send![view, setLayer: layer.as_ref()];
        }

        let shader = device
            .new_library_with_source(SHADER_SOURCE, &metal::CompileOptions::new())
            .map_err(|error| anyhow::anyhow!("Metal shader compilation failed: {error}"))?;
        let descriptor = RenderPipelineDescriptor::new();
        let vertex = shader
            .get_function("video_vertex", None)
            .map_err(|error| anyhow::anyhow!(error))?;
        let fragment = shader
            .get_function("video_fragment", None)
            .map_err(|error| anyhow::anyhow!(error))?;
        descriptor.set_vertex_function(Some(&vertex));
        descriptor.set_fragment_function(Some(&fragment));
        descriptor
            .color_attachments()
            .object_at(0)
            .expect("color attachment")
            .set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        let pipeline = device
            .new_render_pipeline_state(&descriptor)
            .map_err(|error| anyhow::anyhow!("Metal pipeline creation failed: {error}"))?;

        let mut texture_cache = ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCacheCreate(
                ptr::null(),
                ptr::null(),
                device.as_ptr().cast::<c_void>(),
                ptr::null(),
                &mut texture_cache,
            )
        };
        if status != 0 || texture_cache.is_null() {
            bail!("CVMetalTextureCacheCreate failed with status {status}");
        }

        let renderer = Self {
            command_queue: device.new_command_queue(),
            layer,
            pipeline,
            texture_cache,
            rotation: AtomicU32::new(DisplayRotation::Portrait.shader_mode()),
        };
        renderer.resize(window.inner_size().width, window.inner_size().height);
        Ok(renderer)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(window: &winit::window::Window) -> Result<Self> {
        let state = LinuxRendererState::new(window)?;
        Ok(Self {
            linux: std::sync::Mutex::new(state),
            rotation: AtomicU32::new(DisplayRotation::Portrait.shader_mode()),
        })
    }

    pub fn set_rotation(&self, rotation: DisplayRotation) {
        self.rotation
            .store(rotation.shader_mode(), Ordering::Relaxed);
    }

    pub fn resize(&self, width: u32, height: u32) {
        #[cfg(target_os = "macos")]
        self.layer
            .set_drawable_size(CGSize::new(width.max(1) as f64, height.max(1) as f64));
        #[cfg(not(target_os = "macos"))]
        if let Ok(mut state) = self.linux.lock() {
            state.resize(width.max(1), height.max(1));
        }
    }

    pub fn render_frame(&self, frame: &DecodedFrame) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            objc::rc::autoreleasepool(|| -> Result<()> {
                let planes = unsafe { CVPixelBufferGetPlaneCount(frame.as_ptr()) };
                if planes < 2 {
                    bail!("Expected a bi-planar NV12 VideoToolbox frame, got {planes} planes");
                }
                let luma =
                    CvMetalTexture::new(self.texture_cache, frame, 0, MTLPixelFormat::R8Unorm)?;
                let chroma =
                    CvMetalTexture::new(self.texture_cache, frame, 1, MTLPixelFormat::RG8Unorm)?;
                let drawable = self
                    .layer
                    .next_drawable()
                    .context("No Metal drawable available")?;
                let pass = RenderPassDescriptor::new();
                let attachment = pass
                    .color_attachments()
                    .object_at(0)
                    .expect("color attachment");
                attachment.set_texture(Some(drawable.texture()));
                attachment.set_load_action(MTLLoadAction::Clear);
                attachment.set_clear_color(metal::MTLClearColor::new(0.025, 0.025, 0.03, 1.0));
                attachment.set_store_action(MTLStoreAction::Store);

                let command = self.command_queue.new_command_buffer();
                let encoder = command.new_render_command_encoder(pass);
                encoder.set_render_pipeline_state(&self.pipeline);
                let rotation =
                    DisplayRotation::from_shader_mode(self.rotation.load(Ordering::Relaxed));
                let native_landscape = frame.width() > frame.height();
                let (display_width, display_height) =
                    if rotation.is_landscape() && !native_landscape {
                        (frame.height() as f64, frame.width() as f64)
                    } else {
                        (frame.width() as f64, frame.height() as f64)
                    };
                let drawable_size = self.layer.drawable_size();
                let video_aspect = display_width / display_height;
                let drawable_aspect = drawable_size.width / drawable_size.height;
                let (viewport_width, viewport_height) = if drawable_aspect > video_aspect {
                    (drawable_size.height * video_aspect, drawable_size.height)
                } else {
                    (drawable_size.width, drawable_size.width / video_aspect)
                };
                encoder.set_viewport(MTLViewport {
                    originX: (drawable_size.width - viewport_width) * 0.5,
                    originY: (drawable_size.height - viewport_height) * 0.5,
                    width: viewport_width,
                    height: viewport_height,
                    znear: 0.0,
                    zfar: 1.0,
                });
                let shader_rot = if native_landscape && rotation.is_landscape() {
                    0_u32
                } else {
                    rotation.shader_mode()
                };
                encoder.set_fragment_texture(0, Some(luma.texture()));
                encoder.set_fragment_texture(1, Some(chroma.texture()));
                encoder.set_fragment_bytes(
                    0,
                    std::mem::size_of::<u32>() as u64,
                    (&shader_rot as *const u32).cast(),
                );
                encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
                encoder.end_encoding();
                command.present_drawable(drawable);
                command.commit();
                Ok(())
            })?;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let rotation =
                DisplayRotation::from_shader_mode(self.rotation.load(Ordering::Relaxed));
            if let Ok(mut state) = self.linux.lock() {
                state.render_frame(frame, rotation)?;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
struct CvMetalTexture {
    raw: CVMetalTextureRef,
}

#[cfg(target_os = "macos")]
impl CvMetalTexture {
    fn new(
        cache: CVMetalTextureCacheRef,
        frame: &DecodedFrame,
        plane: usize,
        format: MTLPixelFormat,
    ) -> Result<Self> {
        let width = unsafe { CVPixelBufferGetWidthOfPlane(frame.as_ptr(), plane) };
        let height = unsafe { CVPixelBufferGetHeightOfPlane(frame.as_ptr(), plane) };
        let mut raw = ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCacheCreateTextureFromImage(
                ptr::null(),
                cache,
                frame.as_ptr(),
                ptr::null(),
                format as u64,
                width,
                height,
                plane,
                &mut raw,
            )
        };
        if status != 0 || raw.is_null() {
            bail!("CVMetalTexture creation failed for plane {plane}: {status}");
        }
        Ok(Self { raw })
    }

    fn texture(&self) -> &TextureRef {
        let raw = unsafe { CVMetalTextureGetTexture(self.raw) };
        unsafe { &*(raw as *const TextureRef) }
    }
}

#[cfg(target_os = "macos")]
impl Drop for CvMetalTexture {
    fn drop(&mut self) {
        unsafe { CFRelease(self.raw.cast()) }
    }
}

#[cfg(target_os = "macos")]
impl Drop for MetalRenderer {
    fn drop(&mut self) {
        unsafe { CFRelease(self.texture_cache.cast()) }
    }
}

#[cfg(target_os = "macos")]
const SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct Out { float4 position [[position]]; float2 uv; };
vertex Out video_vertex(uint id [[vertex_id]]) {
    float2 positions[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };
    float2 uvs[3] = { float2(0.0, 1.0), float2(2.0, 1.0), float2(0.0, -1.0) };
    return { float4(positions[id], 0.0, 1.0), uvs[id] };
}
fragment float4 video_fragment(Out in [[stage_in]], texture2d<float> yTex [[texture(0)]],
                               texture2d<float> uvTex [[texture(1)]],
                               constant uint &rot [[buffer(0)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    float2 sample_uv = in.uv;
    if (rot == 1) {
        sample_uv = float2(1.0 - in.uv.y, in.uv.x);
    } else if (rot == 2) {
        sample_uv = float2(in.uv.y, 1.0 - in.uv.x);
    } else if (rot == 3) {
        sample_uv = float2(1.0 - in.uv.x, 1.0 - in.uv.y);
    }
    float y = yTex.sample(s, sample_uv).r;
    float2 uv = uvTex.sample(s, sample_uv).rg - float2(0.5);
    float3 rgb = float3(y + 1.5748 * uv.y,
                        y - 0.1873 * uv.x - 0.4681 * uv.y,
                        y + 1.8556 * uv.x);
    return float4(rgb, 1.0);
}
"#;

// ============================================================================
// Linux Runtime-Loaded Wayland (wl_shm) & X11 (XPutImage) Renderer
// ============================================================================

#[cfg(not(target_os = "macos"))]
mod linux_impl {
    use super::*;
    use std::{
        ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString},
        ptr,
    };

    const RTLD_LAZY: c_int = 0x00001;
    const PROT_READ: c_int = 0x1;
    const PROT_WRITE: c_int = 0x2;
    const MAP_SHARED: c_int = 0x01;
    const CLEAR_XRGB: u32 = 0xFF06_0608;

    #[cfg(target_os = "linux")]
    extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
        fn malloc(size: usize) -> *mut c_void;
        fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
        fn ftruncate(fd: c_int, length: i64) -> c_int;
        fn mmap(
            addr: *mut c_void,
            length: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, length: usize) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    #[cfg(not(target_os = "linux"))]
    unsafe fn dlopen(_filename: *const c_char, _flags: c_int) -> *mut c_void {
        ptr::null_mut()
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn dlsym(_handle: *mut c_void, _symbol: *const c_char) -> *mut c_void {
        ptr::null_mut()
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn dlclose(_handle: *mut c_void) -> c_int {
        0
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn malloc(_size: usize) -> *mut c_void {
        ptr::null_mut()
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn memfd_create(_name: *const c_char, _flags: c_uint) -> c_int {
        -1
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn ftruncate(_fd: c_int, _length: i64) -> c_int {
        -1
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn mmap(
        _addr: *mut c_void,
        _length: usize,
        _prot: c_int,
        _flags: c_int,
        _fd: c_int,
        _offset: i64,
    ) -> *mut c_void {
        ptr::null_mut()
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn munmap(_addr: *mut c_void, _length: usize) -> c_int {
        0
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn close(_fd: c_int) -> c_int {
        0
    }

    unsafe fn open_any_lib(names: &[&[u8]]) -> Option<*mut c_void> {
        for &name in names {
            let h = unsafe { dlopen(name.as_ptr().cast(), RTLD_LAZY) };
            if !h.is_null() {
                return Some(h);
            }
        }
        None
    }

    unsafe fn load_sym<T>(lib: *mut c_void, name: &[u8]) -> Result<T> {
        let ptr = unsafe { dlsym(lib, name.as_ptr().cast()) };
        if ptr.is_null() {
            let s = CStr::from_bytes_with_nul(name)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            bail!("Required dynamic symbol missing: {s}");
        }
        Ok(unsafe { std::mem::transmute_copy(&ptr) })
    }

    // ---- X11 Backend --------------------------------------------------------

    struct X11Backend {
        lib: *mut c_void,
        display: *mut c_void,
        window: c_ulong,
        visual: *mut c_void,
        depth: c_int,
        gc: *mut c_void,
        image: *mut c_void,
        pixel_buf: *mut u32,
        width: u32,
        height: u32,
        x_create_image: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            c_uint,
            c_int,
            c_int,
            *mut c_char,
            c_uint,
            c_uint,
            c_int,
            c_int,
        ) -> *mut c_void,
        x_put_image: unsafe extern "C" fn(
            *mut c_void,
            c_ulong,
            *mut c_void,
            *mut c_void,
            c_int,
            c_int,
            c_int,
            c_int,
            c_uint,
            c_uint,
        ) -> c_int,
        x_destroy_image: unsafe extern "C" fn(*mut c_void) -> c_int,
        x_free_gc: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
        x_flush: unsafe extern "C" fn(*mut c_void) -> c_int,
    }

    unsafe impl Send for X11Backend {}

    impl X11Backend {
        fn new(display: *mut c_void, window: c_ulong, width: u32, height: u32) -> Result<Self> {
            unsafe {
                let lib = open_any_lib(&[b"libX11.so.6\0", b"libX11.so\0"])
                    .context("Failed to dynamically open libX11.so.6")?;
                let x_default_screen: unsafe extern "C" fn(*mut c_void) -> c_int =
                    load_sym(lib, b"XDefaultScreen\0")?;
                let x_default_visual: unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void =
                    load_sym(lib, b"XDefaultVisual\0")?;
                let x_default_depth: unsafe extern "C" fn(*mut c_void, c_int) -> c_int =
                    load_sym(lib, b"XDefaultDepth\0")?;
                let x_create_gc: unsafe extern "C" fn(
                    *mut c_void,
                    c_ulong,
                    c_ulong,
                    *mut c_void,
                ) -> *mut c_void = load_sym(lib, b"XCreateGC\0")?;

                let screen = x_default_screen(display);
                let visual = x_default_visual(display, screen);
                let depth = x_default_depth(display, screen);
                let gc = x_create_gc(display, window, 0, ptr::null_mut());
                if gc.is_null() {
                    dlclose(lib);
                    bail!("XCreateGC returned null");
                }

                let mut backend = Self {
                    lib,
                    display,
                    window,
                    visual,
                    depth,
                    gc,
                    image: ptr::null_mut(),
                    pixel_buf: ptr::null_mut(),
                    width: 0,
                    height: 0,
                    x_create_image: load_sym(lib, b"XCreateImage\0")?,
                    x_put_image: load_sym(lib, b"XPutImage\0")?,
                    x_destroy_image: load_sym(lib, b"XDestroyImage\0")?,
                    x_free_gc: load_sym(lib, b"XFreeGC\0")?,
                    x_flush: load_sym(lib, b"XFlush\0")?,
                };
                backend.resize(width.max(1), height.max(1));
                Ok(backend)
            }
        }

        fn resize(&mut self, width: u32, height: u32) {
            if self.width == width && self.height == height && !self.image.is_null() {
                return;
            }
            unsafe {
                if !self.image.is_null() {
                    (self.x_destroy_image)(self.image);
                    self.image = ptr::null_mut();
                    self.pixel_buf = ptr::null_mut();
                }
                let bytes = (width as usize) * (height as usize) * 4;
                let data = malloc(bytes);
                if data.is_null() {
                    return;
                }
                // ZPixmap = 2
                let image = (self.x_create_image)(
                    self.display,
                    self.visual,
                    self.depth as c_uint,
                    2,
                    0,
                    data.cast(),
                    width,
                    height,
                    32,
                    (width * 4) as c_int,
                );
                if image.is_null() {
                    return;
                }
                self.image = image;
                self.pixel_buf = data.cast();
                self.width = width;
                self.height = height;
            }
        }

        fn present(&mut self, frame: &DecodedFrame, rotation: DisplayRotation) -> Result<()> {
            if self.image.is_null() || self.pixel_buf.is_null() {
                return Ok(());
            }
            let Some(cpu) = frame.cpu_frame() else {
                return Ok(());
            };
            let len = (self.width as usize) * (self.height as usize);
            let dst = unsafe { std::slice::from_raw_parts_mut(self.pixel_buf, len) };
            render_nv12_to_xrgb(cpu, rotation, self.width as usize, self.height as usize, dst);
            unsafe {
                (self.x_put_image)(
                    self.display,
                    self.window,
                    self.gc,
                    self.image,
                    0,
                    0,
                    0,
                    0,
                    self.width,
                    self.height,
                );
                (self.x_flush)(self.display);
            }
            Ok(())
        }
    }

    impl Drop for X11Backend {
        fn drop(&mut self) {
            unsafe {
                if !self.image.is_null() {
                    (self.x_destroy_image)(self.image);
                }
                if !self.gc.is_null() {
                    (self.x_free_gc)(self.display, self.gc);
                }
                if !self.lib.is_null() {
                    dlclose(self.lib);
                }
            }
        }
    }

    // ---- Wayland wl_shm Backend ---------------------------------------------

    #[repr(C)]
    #[derive(Clone, Copy)]
    union WlArgument {
        i: i32,
        u: u32,
        s: *const c_char,
        o: *mut c_void,
        n: u32,
        h: i32,
    }

    #[repr(C)]
    struct WlRegistryListener {
        global: unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const c_char, u32),
        global_remove: unsafe extern "C" fn(*mut c_void, *mut c_void, u32),
    }

    struct WaylandDiscovery {
        shm_name: Option<u32>,
        shm_version: u32,
    }

    unsafe extern "C" fn registry_global_cb(
        data: *mut c_void,
        _registry: *mut c_void,
        name: u32,
        interface: *const c_char,
        version: u32,
    ) {
        if data.is_null() || interface.is_null() {
            return;
        }
        let state = unsafe { &mut *(data as *mut WaylandDiscovery) };
        let Ok(iface) = unsafe { CStr::from_ptr(interface) }.to_str() else {
            return;
        };
        if iface == "wl_shm" {
            state.shm_name = Some(name);
            state.shm_version = version.min(1);
        }
    }

    unsafe extern "C" fn registry_global_remove_cb(
        _data: *mut c_void,
        _registry: *mut c_void,
        _name: u32,
    ) {
    }

    struct WaylandBackend {
        lib: *mut c_void,
        display: *mut c_void,
        surface: *mut c_void,
        shm: *mut c_void,
        wl_shm_pool_interface: *const c_void,
        wl_buffer_interface: *const c_void,
        proxy_marshal_array:
            unsafe extern "C" fn(*mut c_void, u32, *mut WlArgument),
        proxy_marshal_array_constructor: unsafe extern "C" fn(
            *mut c_void,
            u32,
            *mut WlArgument,
            *const c_void,
        ) -> *mut c_void,
        proxy_destroy: unsafe extern "C" fn(*mut c_void),
        display_flush: unsafe extern "C" fn(*mut c_void) -> c_int,
        fd: c_int,
        mmap_ptr: *mut u8,
        mmap_len: usize,
        buffers: [*mut c_void; 2],
        slot: usize,
        width: u32,
        height: u32,
    }

    unsafe impl Send for WaylandBackend {}

    impl WaylandBackend {
        fn new(display: *mut c_void, surface: *mut c_void, width: u32, height: u32) -> Result<Self> {
            unsafe {
                let lib = open_any_lib(&[b"libwayland-client.so.0\0", b"libwayland-client.so\0"])
                    .context("Failed to dynamically open libwayland-client.so.0")?;
                let wl_registry_interface: *const c_void =
                    load_sym(lib, b"wl_registry_interface\0")?;
                let wl_shm_interface: *const c_void = load_sym(lib, b"wl_shm_interface\0")?;
                let wl_shm_pool_interface: *const c_void =
                    load_sym(lib, b"wl_shm_pool_interface\0")?;
                let wl_buffer_interface: *const c_void =
                    load_sym(lib, b"wl_buffer_interface\0")?;

                let proxy_marshal_array: unsafe extern "C" fn(*mut c_void, u32, *mut WlArgument) =
                    load_sym(lib, b"wl_proxy_marshal_array\0")?;
                let proxy_marshal_array_constructor: unsafe extern "C" fn(
                    *mut c_void,
                    u32,
                    *mut WlArgument,
                    *const c_void,
                ) -> *mut c_void =
                    load_sym(lib, b"wl_proxy_marshal_array_constructor\0")?;
                let proxy_add_listener: unsafe extern "C" fn(
                    *mut c_void,
                    *const c_void,
                    *mut c_void,
                ) -> c_int = load_sym(lib, b"wl_proxy_add_listener\0")?;
                let proxy_destroy: unsafe extern "C" fn(*mut c_void) =
                    load_sym(lib, b"wl_proxy_destroy\0")?;
                let display_roundtrip: unsafe extern "C" fn(*mut c_void) -> c_int =
                    load_sym(lib, b"wl_display_roundtrip\0")?;
                let display_flush: unsafe extern "C" fn(*mut c_void) -> c_int =
                    load_sym(lib, b"wl_display_flush\0")?;

                // wl_display opcode 1 = get_registry(new_id)
                let mut reg_args = [WlArgument { n: 0 }];
                let registry = proxy_marshal_array_constructor(
                    display,
                    1,
                    reg_args.as_mut_ptr(),
                    wl_registry_interface,
                );
                if registry.is_null() {
                    dlclose(lib);
                    bail!("Wayland wl_display_get_registry failed");
                }

                let mut discovery = Box::new(WaylandDiscovery {
                    shm_name: None,
                    shm_version: 1,
                });
                let listener = Box::new(WlRegistryListener {
                    global: registry_global_cb,
                    global_remove: registry_global_remove_cb,
                });
                proxy_add_listener(
                    registry,
                    (&*listener as *const WlRegistryListener).cast(),
                    (&mut *discovery as *mut WaylandDiscovery).cast(),
                );
                display_roundtrip(display);

                let Some(shm_name) = discovery.shm_name else {
                    proxy_destroy(registry);
                    dlclose(lib);
                    bail!("Wayland compositor does not advertise wl_shm");
                };

                // wl_registry opcode 0 = bind(name, interface_name, version, new_id)
                let iface_name = CString::new("wl_shm").unwrap();
                let mut bind_args = [
                    WlArgument { u: shm_name },
                    WlArgument {
                        s: iface_name.as_ptr(),
                    },
                    WlArgument {
                        u: discovery.shm_version,
                    },
                    WlArgument { n: 0 },
                ];
                let shm = proxy_marshal_array_constructor(
                    registry,
                    0,
                    bind_args.as_mut_ptr(),
                    wl_shm_interface,
                );
                proxy_destroy(registry);
                if shm.is_null() {
                    dlclose(lib);
                    bail!("Failed to bind Wayland wl_shm global");
                }

                let mut backend = Self {
                    lib,
                    display,
                    surface,
                    shm,
                    wl_shm_pool_interface,
                    wl_buffer_interface,
                    proxy_marshal_array,
                    proxy_marshal_array_constructor,
                    proxy_destroy,
                    display_flush,
                    fd: -1,
                    mmap_ptr: ptr::null_mut(),
                    mmap_len: 0,
                    buffers: [ptr::null_mut(), ptr::null_mut()],
                    slot: 0,
                    width: 0,
                    height: 0,
                };
                backend.resize(width.max(1), height.max(1));
                Ok(backend)
            }
        }

        fn release_buffers(&mut self) {
            unsafe {
                for buf in &mut self.buffers {
                    if !buf.is_null() {
                        (self.proxy_destroy)(*buf);
                        *buf = ptr::null_mut();
                    }
                }
                if !self.mmap_ptr.is_null() {
                    munmap(self.mmap_ptr.cast(), self.mmap_len);
                    self.mmap_ptr = ptr::null_mut();
                    self.mmap_len = 0;
                }
                if self.fd >= 0 {
                    close(self.fd);
                    self.fd = -1;
                }
            }
        }

        fn resize(&mut self, width: u32, height: u32) {
            if self.width == width && self.height == height && !self.buffers[0].is_null() {
                return;
            }
            self.release_buffers();
            let stride = (width as usize) * 4;
            let frame_bytes = stride * (height as usize);
            let total_bytes = frame_bytes * 2;
            unsafe {
                let fd = memfd_create(b"iphone-mirror-shm\0".as_ptr().cast(), 0);
                if fd < 0 {
                    return;
                }
                if ftruncate(fd, total_bytes as i64) != 0 {
                    close(fd);
                    return;
                }
                let ptr_mem = mmap(
                    ptr::null_mut(),
                    total_bytes,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd,
                    0,
                );
                if ptr_mem.is_null() || ptr_mem == (-1_isize as *mut c_void) {
                    close(fd);
                    return;
                }

                // wl_shm opcode 0 = create_pool(new_id, fd, size)
                let mut pool_args = [
                    WlArgument { n: 0 },
                    WlArgument { h: fd },
                    WlArgument {
                        i: total_bytes as i32,
                    },
                ];
                let pool = (self.proxy_marshal_array_constructor)(
                    self.shm,
                    0,
                    pool_args.as_mut_ptr(),
                    self.wl_shm_pool_interface,
                );
                if pool.is_null() {
                    munmap(ptr_mem, total_bytes);
                    close(fd);
                    return;
                }

                // WL_SHM_FORMAT_XRGB8888 = 1
                for i in 0..2 {
                    let mut buf_args = [
                        WlArgument { n: 0 },
                        WlArgument {
                            i: (i * frame_bytes) as i32,
                        },
                        WlArgument { i: width as i32 },
                        WlArgument { i: height as i32 },
                        WlArgument { i: stride as i32 },
                        WlArgument { u: 1 },
                    ];
                    self.buffers[i] = (self.proxy_marshal_array_constructor)(
                        pool,
                        0,
                        buf_args.as_mut_ptr(),
                        self.wl_buffer_interface,
                    );
                }
                (self.proxy_destroy)(pool);

                self.fd = fd;
                self.mmap_ptr = ptr_mem.cast();
                self.mmap_len = total_bytes;
                self.width = width;
                self.height = height;
            }
        }

        fn present(&mut self, frame: &DecodedFrame, rotation: DisplayRotation) -> Result<()> {
            if self.mmap_ptr.is_null() || self.buffers[self.slot].is_null() {
                return Ok(());
            }
            let Some(cpu) = frame.cpu_frame() else {
                return Ok(());
            };
            let pixels = (self.width as usize) * (self.height as usize);
            let frame_bytes = pixels * 4;
            let offset = self.slot * frame_bytes;
            let dst = unsafe {
                std::slice::from_raw_parts_mut(self.mmap_ptr.add(offset).cast::<u32>(), pixels)
            };
            render_nv12_to_xrgb(cpu, rotation, self.width as usize, self.height as usize, dst);

            let buffer = self.buffers[self.slot];
            self.slot = 1 - self.slot;

            unsafe {
                // wl_surface opcode 1 = attach(buffer, x, y)
                let mut attach_args = [
                    WlArgument { o: buffer },
                    WlArgument { i: 0 },
                    WlArgument { i: 0 },
                ];
                (self.proxy_marshal_array)(self.surface, 1, attach_args.as_mut_ptr());

                // wl_surface opcode 2 = damage(x, y, width, height)
                let mut damage_args = [
                    WlArgument { i: 0 },
                    WlArgument { i: 0 },
                    WlArgument {
                        i: self.width as i32,
                    },
                    WlArgument {
                        i: self.height as i32,
                    },
                ];
                (self.proxy_marshal_array)(self.surface, 2, damage_args.as_mut_ptr());

                // wl_surface opcode 6 = commit()
                (self.proxy_marshal_array)(self.surface, 6, ptr::null_mut());
                (self.display_flush)(self.display);
            }
            Ok(())
        }
    }

    impl Drop for WaylandBackend {
        fn drop(&mut self) {
            self.release_buffers();
            unsafe {
                if !self.shm.is_null() {
                    (self.proxy_destroy)(self.shm);
                }
                if !self.lib.is_null() {
                    dlclose(self.lib);
                }
            }
        }
    }

    pub enum LinuxBackend {
        Wayland(WaylandBackend),
        X11(X11Backend),
        Null,
    }

    pub struct LinuxRendererState {
        backend: LinuxBackend,
    }

    impl LinuxRendererState {
        pub fn new(window: &winit::window::Window) -> Result<Self> {
            use winit::raw_window_handle::{
                HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
            };
            let size = window.inner_size();
            let raw_disp = window
                .display_handle()
                .context("Missing display handle")?
                .as_raw();
            let raw_win = window
                .window_handle()
                .context("Missing window handle")?
                .as_raw();

            let backend = match (raw_disp, raw_win) {
                (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => {
                    tracing::info!("Initializing native Wayland (wl_shm) mirror renderer");
                    LinuxBackend::Wayland(WaylandBackend::new(
                        d.display.as_ptr(),
                        w.surface.as_ptr(),
                        size.width,
                        size.height,
                    )?)
                }
                (RawDisplayHandle::Xlib(d), RawWindowHandle::Xlib(w)) => {
                    tracing::info!("Initializing native X11 (XPutImage) mirror renderer");
                    let disp = d
                        .display
                        .context("Xlib RawDisplayHandle had null display")?
                        .as_ptr();
                    LinuxBackend::X11(X11Backend::new(disp, w.window, size.width, size.height)?)
                }
                _ => {
                    tracing::warn!("Unsupported Linux display handle; using headless presenter");
                    LinuxBackend::Null
                }
            };
            Ok(Self { backend })
        }

        pub fn resize(&mut self, width: u32, height: u32) {
            match &mut self.backend {
                LinuxBackend::Wayland(w) => w.resize(width, height),
                LinuxBackend::X11(x) => x.resize(width, height),
                LinuxBackend::Null => {}
            }
        }

        pub fn render_frame(&mut self, frame: &DecodedFrame, rotation: DisplayRotation) -> Result<()> {
            match &mut self.backend {
                LinuxBackend::Wayland(w) => w.present(frame, rotation),
                LinuxBackend::X11(x) => x.present(frame, rotation),
                LinuxBackend::Null => Ok(()),
            }
        }
    }

    /// Fast fixed-point NV12 -> XRGB8888 (`0xFFRRGGBB`) renderer with aspect-fit letterboxing
    /// and 90/180/270 degree rotation.
    pub fn render_nv12_to_xrgb(
        cpu: &crate::codec::decoder::CpuFrame,
        rotation: DisplayRotation,
        dst_w: usize,
        dst_h: usize,
        dst: &mut [u32],
    ) {
        dst.fill(CLEAR_XRGB);
        if dst_w == 0 || dst_h == 0 || cpu.width == 0 || cpu.height == 0 {
            return;
        }
        let luma_len = cpu.width * cpu.height;
        if cpu.nv12.len() < luma_len + luma_len / 2 {
            return;
        }
        let y_plane = &cpu.nv12[..luma_len];
        let uv_plane = &cpu.nv12[luma_len..luma_len + luma_len / 2];

        let native_landscape = cpu.width > cpu.height;
        let (disp_w, disp_h) = if rotation.is_landscape() && !native_landscape {
            (cpu.height as f64, cpu.width as f64)
        } else {
            (cpu.width as f64, cpu.height as f64)
        };
        let shader_rot = if native_landscape && rotation.is_landscape() {
            0_u32
        } else {
            rotation.shader_mode()
        };

        let video_aspect = disp_w / disp_h;
        let win_aspect = dst_w as f64 / dst_h as f64;
        let (vp_w, vp_h) = if win_aspect > video_aspect {
            ((dst_h as f64 * video_aspect).round() as usize, dst_h)
        } else {
            (dst_w, (dst_w as f64 / video_aspect).round() as usize)
        };
        let vp_w = vp_w.max(1).min(dst_w);
        let vp_h = vp_h.max(1).min(dst_h);
        let vp_x = (dst_w - vp_w) / 2;
        let vp_y = (dst_h - vp_h) / 2;

        let src_w = cpu.width;
        let src_h = cpu.height;

        for dy in 0..vp_h {
            let v_q16 = (dy * 65536) / vp_h;
            let out_row = &mut dst[(vp_y + dy) * dst_w + vp_x..(vp_y + dy) * dst_w + vp_x + vp_w];
            for (dx, pixel) in out_row.iter_mut().enumerate() {
                let u_q16 = (dx * 65536) / vp_w;
                let (su_q16, sv_q16) = match shader_rot {
                    1 => (65535 - v_q16, u_q16),
                    2 => (v_q16, 65535 - u_q16),
                    3 => (65535 - u_q16, 65535 - v_q16),
                    _ => (u_q16, v_q16),
                };
                let sx = ((su_q16 * (src_w - 1)) >> 16).min(src_w - 1);
                let sy = ((sv_q16 * (src_h - 1)) >> 16).min(src_h - 1);

                let y_val = y_plane[sy * src_w + sx] as i32;
                let uv_idx = (sy >> 1) * src_w + (sx & !1);
                let cb = uv_plane[uv_idx] as i32 - 128;
                let cr = uv_plane[uv_idx + 1] as i32 - 128;

                // BT.709 video range fixed-point (scaled by 256)
                let c = (y_val - 16).max(0) * 298;
                let r = ((c + 459 * cr + 128) >> 8).clamp(0, 255) as u32;
                let g = ((c - 55 * cb - 136 * cr + 128) >> 8).clamp(0, 255) as u32;
                let b = ((c + 541 * cb + 128) >> 8).clamp(0, 255) as u32;
                *pixel = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
use linux_impl::LinuxRendererState;

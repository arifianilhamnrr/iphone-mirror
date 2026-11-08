// language: Rust, file: src/ui/metal_renderer.rs, target: macOS
use crate::codec::{decoder::DecodedFrame, videotoolbox_sys::*};
use anyhow::{bail, Context, Result};

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
        layer.set_display_sync_enabled(true);

        let handle = window
            .window_handle()
            .context("Could not get AppKit window handle")?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            bail!("Expected an AppKit window handle");
        };
        unsafe {
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
        };
        renderer.resize(window.inner_size().width, window.inner_size().height);
        Ok(renderer)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(_window: &winit::window::Window) -> Result<Self> {
        Ok(Self {})
    }

    pub fn resize(&self, width: u32, height: u32) {
        #[cfg(target_os = "macos")]
        self.layer
            .set_drawable_size(CGSize::new(width.max(1) as f64, height.max(1) as f64));
    }

    pub fn render_frame(&self, frame: &DecodedFrame) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let planes = unsafe { CVPixelBufferGetPlaneCount(frame.as_ptr()) };
            if planes < 2 {
                bail!("Expected a bi-planar NV12 VideoToolbox frame, got {planes} planes");
            }
            let luma = CvMetalTexture::new(self.texture_cache, frame, 0, MTLPixelFormat::R8Unorm)?;
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
            let drawable_size = self.layer.drawable_size();
            let video_aspect = frame.width() as f64 / frame.height() as f64;
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
            encoder.set_fragment_texture(0, Some(luma.texture()));
            encoder.set_fragment_texture(1, Some(chroma.texture()));
            encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
            encoder.end_encoding();
            command.present_drawable(drawable);
            command.commit();
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
                               texture2d<float> uvTex [[texture(1)]]) {
    constexpr sampler s(address::clamp_to_edge, filter::linear);
    float y = yTex.sample(s, in.uv).r;
    float2 uv = uvTex.sample(s, in.uv).rg - float2(0.5);
    float3 rgb = float3(y + 1.5748 * uv.y,
                        y - 0.1873 * uv.x - 0.4681 * uv.y,
                        y + 1.8556 * uv.x);
    return float4(rgb, 1.0);
}
"#;

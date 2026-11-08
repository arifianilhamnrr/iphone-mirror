// language: Rust, file: src/codec/videotoolbox_sys.rs, target: macOS only
//! Native VideoToolbox and CoreMedia FFI declarations for hardware HEVC decoding.
use std::ffi::c_void;

#[repr(C)]
pub struct OpaqueVTDecompressionSession(c_void);
pub type VTDecompressionSessionRef = *mut OpaqueVTDecompressionSession;

#[repr(C)]
pub struct OpaqueCMFormatDescription(c_void);
pub type CMVideoFormatDescriptionRef = *mut OpaqueCMFormatDescription;

#[repr(C)]
pub struct __CVBuffer(c_void);
pub type CVPixelBufferRef = *mut __CVBuffer;

#[repr(C)]
pub struct OpaqueCVMetalTextureCache(c_void);
pub type CVMetalTextureCacheRef = *mut OpaqueCVMetalTextureCache;

#[repr(C)]
pub struct OpaqueCVMetalTexture(c_void);
pub type CVMetalTextureRef = *mut OpaqueCVMetalTexture;

pub type OSStatus = i32;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

pub type VTDecompressionOutputCallback = extern "C" fn(
    decompression_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: CVPixelBufferRef,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
    pub decompression_output_callback: Option<VTDecompressionOutputCallback>,
    pub decompression_output_ref_con: *mut c_void,
}

#[repr(C)]
pub struct OpaqueCMBlockBuffer(c_void);
pub type CMBlockBufferRef = *mut OpaqueCMBlockBuffer;

#[repr(C)]
pub struct OpaqueCMSampleBuffer(c_void);
pub type CMSampleBufferRef = *mut OpaqueCMSampleBuffer;

#[cfg(target_os = "macos")]
#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {}

#[cfg(target_os = "macos")]
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {}

#[cfg(target_os = "macos")]
#[link(name = "CoreVideo", kind = "framework")]
extern "C" {}

#[cfg(target_os = "macos")]
extern "C" {
    pub fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: *mut c_void,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: *mut c_void,
        custom_block_source: *mut c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;

    pub fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;

    pub fn CMSampleBufferCreateReady(
        allocator: *mut c_void,
        data_buffer: CMBlockBufferRef,
        format_description: CMVideoFormatDescriptionRef,
        sample_count: usize,
        sample_timing_entry_count: usize,
        sample_timing_array: *const c_void,
        sample_size_entry_count: usize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;

    pub fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: *mut c_void,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        extensions: *mut c_void,
        format_description_out: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionCreate(
        allocator: *mut c_void,
        video_format_description: CMVideoFormatDescriptionRef,
        video_decoder_specification: *mut c_void,
        destination_image_buffer_attributes: *mut c_void,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;

    pub fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    pub fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;

    pub fn CVPixelBufferRetain(pixel_buffer: CVPixelBufferRef) -> CVPixelBufferRef;
    pub fn CVPixelBufferRelease(pixel_buffer: CVPixelBufferRef);
    pub fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetPlaneCount(pixel_buffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetWidthOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;
    pub fn CVPixelBufferGetHeightOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;
    pub fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *mut c_void;
    pub fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;
    pub fn CVPixelBufferGetPixelFormatType(pixel_buffer: CVPixelBufferRef) -> u32;
    pub fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, flags: u64) -> i32;
    pub fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVPixelBufferRef, flags: u64) -> i32;

    pub fn CVMetalTextureCacheCreate(
        allocator: *const c_void,
        cache_attributes: *const c_void,
        metal_device: *mut c_void,
        texture_attributes: *const c_void,
        cache_out: *mut CVMetalTextureCacheRef,
    ) -> i32;
    pub fn CVMetalTextureCacheCreateTextureFromImage(
        allocator: *const c_void,
        texture_cache: CVMetalTextureCacheRef,
        source_image: CVPixelBufferRef,
        texture_attributes: *const c_void,
        pixel_format: u64,
        width: usize,
        height: usize,
        plane_index: usize,
        texture_out: *mut CVMetalTextureRef,
    ) -> i32;
    pub fn CVMetalTextureGetTexture(texture: CVMetalTextureRef) -> *mut c_void;

    pub fn CFRelease(cf: *const c_void);
}

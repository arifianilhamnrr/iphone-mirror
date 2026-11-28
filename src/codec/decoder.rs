// language: Rust, file: src/codec/decoder.rs
use crate::codec::format_desc::FormatDescription;
#[cfg(not(target_os = "macos"))]
use crate::codec::format_desc::VideoCodec;
use crate::codec::videotoolbox_sys::*;
use anyhow::{bail, Result};
#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::{
    ptr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::mpsc;
use tracing::info;

#[cfg(not(target_os = "macos"))]
use std::{
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::AtomicBool,
    thread::JoinHandle,
};

#[derive(Debug, Clone)]
pub struct CpuFrame {
    pub width: usize,
    pub height: usize,
    pub nv12: Vec<u8>,
    pub full_range: bool,
}

/// A retained decoded output frame (VideoToolbox `CVPixelBuffer` on macOS, or NV12 `CpuFrame` on Linux).
pub struct DecodedFrame {
    pixel_buffer: CVPixelBufferRef,
    cpu_frame: Option<Arc<CpuFrame>>,
    pub presentation_time: CMTime,
    pub duration: CMTime,
}

// CVPixelBuffer is an IOSurface-backed Core Foundation object designed to cross threads.
// CpuFrame is wrapped in Arc and contains plain bytes.
unsafe impl Send for DecodedFrame {}

impl DecodedFrame {
    pub fn from_nv12(width: usize, height: usize, nv12: Vec<u8>, full_range: bool) -> Self {
        Self {
            pixel_buffer: ptr::null_mut(),
            cpu_frame: Some(Arc::new(CpuFrame {
                width,
                height,
                nv12,
                full_range,
            })),
            presentation_time: CMTime::default(),
            duration: CMTime::default(),
        }
    }

    pub fn cpu_frame(&self) -> Option<&Arc<CpuFrame>> {
        self.cpu_frame.as_ref()
    }

    pub fn width(&self) -> usize {
        if let Some(frame) = &self.cpu_frame {
            return frame.width;
        }
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.pixel_buffer.is_null() {
                return CVPixelBufferGetWidth(self.pixel_buffer);
            }
        }
        0
    }

    pub fn height(&self) -> usize {
        if let Some(frame) = &self.cpu_frame {
            return frame.height;
        }
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.pixel_buffer.is_null() {
                return CVPixelBufferGetHeight(self.pixel_buffer);
            }
        }
        0
    }

    pub fn as_ptr(&self) -> CVPixelBufferRef {
        self.pixel_buffer
    }
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.pixel_buffer.is_null() {
                CVPixelBufferRelease(self.pixel_buffer);
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct CallbackContext {
    frame_tx: mpsc::Sender<DecodedFrame>,
    frame_count: Arc<AtomicU64>,
}

#[cfg(not(target_os = "macos"))]
enum LinuxVideoBackend {
    LibAv {
        packet_tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
        worker: Option<JoinHandle<()>>,
    },
    Ffmpeg {
        stdin: Option<ChildStdin>,
        child: Child,
        stop: Arc<AtomicBool>,
        reader_thread: Option<JoinHandle<()>>,
    },
}

pub struct HardwareDecoder {
    #[cfg(target_os = "macos")]
    session: VTDecompressionSessionRef,
    #[cfg(target_os = "macos")]
    _callback_context: Box<CallbackContext>,
    #[cfg(not(target_os = "macos"))]
    backend: LinuxVideoBackend,
    #[cfg(not(target_os = "macos"))]
    needs_params: bool,
    _format_desc: FormatDescription,
}

// VTDecompressionSession accepts decode submissions from a worker task and owns
// its callback synchronization. On Linux, Child + ChildStdin are owned exclusively by this task.
unsafe impl Send for HardwareDecoder {}

impl HardwareDecoder {
    #[cfg(target_os = "macos")]
    extern "C" fn decode_callback(
        decompression_output_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: OSStatus,
        _info_flags: u32,
        image_buffer: CVPixelBufferRef,
        presentation_time: CMTime,
        duration: CMTime,
    ) {
        if status != 0 || image_buffer.is_null() {
            tracing::warn!(
                status,
                null_buf = image_buffer.is_null(),
                "VideoToolbox output callback reported an error"
            );
            return;
        }

        let context = unsafe { &*(decompression_output_ref_con as *const CallbackContext) };
        context.frame_count.fetch_add(1, Ordering::Relaxed);
        let retained = unsafe { CVPixelBufferRetain(image_buffer) };
        let frame = DecodedFrame {
            pixel_buffer: retained,
            cpu_frame: None,
            presentation_time,
            duration,
        };

        if let Err(error) = context.frame_tx.try_send(frame) {
            tracing::debug!(%error, "Dropping decoded frame because the renderer is behind");
        }
    }

    pub fn new(
        format_desc: FormatDescription,
        frame_tx: mpsc::Sender<DecodedFrame>,
        frame_count: Arc<AtomicU64>,
    ) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let mut callback_context = Box::new(CallbackContext {
                frame_tx,
                frame_count,
            });
            let callback_record = VTDecompressionOutputCallbackRecord {
                decompression_output_callback: Some(Self::decode_callback),
                decompression_output_ref_con: (&mut *callback_context as *mut CallbackContext)
                    .cast(),
            };
            let mut session: VTDecompressionSessionRef = ptr::null_mut();

            let status = unsafe {
                VTDecompressionSessionCreate(
                    ptr::null_mut(),
                    format_desc.raw,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &callback_record,
                    &mut session,
                )
            };

            if status != 0 || session.is_null() {
                bail!("VTDecompressionSessionCreate failed with OSStatus {status}");
            }

            info!("Hardware VideoToolbox decompression session initialized");
            Ok(Self {
                session,
                _format_desc: format_desc,
                _callback_context: callback_context,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let (width, height) = format_desc.dimensions;
            if width == 0 || height == 0 {
                bail!("Invalid decoded frame dimensions {width}x{height}");
            }

            match libav_video::LibAvVideoContext::new(format_desc.codec) {
                Ok(mut libav) => {
                    if !format_desc.annex_b_params.is_empty() {
                        libav.send_annex_b(&format_desc.annex_b_params, |_, _, _, _, _| {});
                    }
                    let (packet_tx, packet_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(12);
                    let worker = std::thread::Builder::new()
                        .name("linux-video-decoder".to_string())
                        .spawn(move || {
                            while let Ok(annex_b) = packet_rx.recv() {
                                libav.send_annex_b(&annex_b, |w, h, nv12, full_range, _| {
                                    frame_count.fetch_add(1, Ordering::Relaxed);
                                    let frame = DecodedFrame::from_nv12(w, h, nv12, full_range);
                                    if let Err(error) = frame_tx.try_send(frame) {
                                        tracing::debug!(
                                            %error,
                                            "Dropping decoded frame because the renderer is behind"
                                        );
                                    }
                                });
                            }
                        })?;

                    info!(
                        width,
                        height,
                        codec = format_desc.codec.ffmpeg_format(),
                        "Linux in-process libavcodec video decoder initialized"
                    );
                    let needs_params = !format_desc.annex_b_params.is_empty();
                    return Ok(Self {
                        backend: LinuxVideoBackend::LibAv {
                            packet_tx: Some(packet_tx),
                            worker: Some(worker),
                        },
                        needs_params,
                        _format_desc: format_desc,
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "libavcodec unavailable; falling back to ffmpeg video subprocess");
                }
            }

            let frame_bytes = width * height * 3 / 2;
            let codec_fmt = format_desc.codec.ffmpeg_format();

            let mut child = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-threads",
                    "1",
                    "-flags",
                    "low_delay",
                    "-probesize",
                    "4096",
                    "-analyzeduration",
                    "0",
                    "-fpsprobesize",
                    "0",
                    "-f",
                    codec_fmt,
                    "-i",
                    "pipe:0",
                    "-vf",
                    &format!("scale={width}:{height}"),
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "nv12",
                    "pipe:1",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|err| {
                    anyhow::anyhow!(
                        "Failed to start ffmpeg video decoder ({codec_fmt}): {err}. Install ffmpeg for Linux video decoding."
                    )
                })?;

            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg stdin pipe unavailable"))?;
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("ffmpeg stdout pipe unavailable"))?;

            if !format_desc.annex_b_params.is_empty() {
                stdin.write_all(&format_desc.annex_b_params)?;
                stdin.flush()?;
            }

            let stop = Arc::new(AtomicBool::new(false));
            let stop_reader = stop.clone();
            let reader_thread = std::thread::Builder::new()
                .name("linux-video-decoder".to_string())
                .spawn(move || {
                    while !stop_reader.load(Ordering::Acquire) {
                        let mut buf = vec![0_u8; frame_bytes];
                        if stdout.read_exact(&mut buf).is_err() {
                            break;
                        }
                        frame_count.fetch_add(1, Ordering::Relaxed);
                        let frame = DecodedFrame::from_nv12(width, height, buf, false);
                        if let Err(error) = frame_tx.try_send(frame) {
                            tracing::debug!(%error, "Dropping decoded frame because the renderer is behind");
                        }
                    }
                })?;

            info!(
                width,
                height,
                codec = codec_fmt,
                "Linux ffmpeg video decoder initialized"
            );
            Ok(Self {
                backend: LinuxVideoBackend::Ffmpeg {
                    stdin: Some(stdin),
                    child,
                    stop,
                    reader_thread: Some(reader_thread),
                },
                needs_params: false,
                _format_desc: format_desc,
            })
        }
    }

    /// Feed one complete Annex-B NAL unit to the decoder.
    pub fn decode_nal(&mut self, nal: &[u8]) -> Result<()> {
        let payload = annex_b_payload(nal)?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            anyhow::anyhow!("HEVC NAL is too large for its 4-byte AVCC length prefix")
        })?;
        let mut avcc = Vec::with_capacity(payload.len() + 4);
        avcc.extend_from_slice(&payload_len.to_be_bytes());
        avcc.extend_from_slice(payload);
        self.decode_avcc(&avcc)
    }

    /// Feed an AVCC length-prefixed access unit directly to the decoder.
    pub fn decode_avcc(&mut self, avcc: &[u8]) -> Result<()> {
        if avcc.is_empty() {
            return Ok(());
        }

        #[cfg(target_os = "macos")]
        {
            let mut block_buffer: CMBlockBufferRef = ptr::null_mut();
            let status = unsafe {
                // A null memory block asks CoreMedia to allocate storage it owns.
                CMBlockBufferCreateWithMemoryBlock(
                    ptr::null_mut(),
                    ptr::null_mut(),
                    avcc.len(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    avcc.len(),
                    0,
                    &mut block_buffer,
                )
            };
            if status != 0 || block_buffer.is_null() {
                bail!("CMBlockBufferCreateWithMemoryBlock failed with OSStatus {status}");
            }
            let block_buffer = CfGuard::new(block_buffer);

            let status = unsafe {
                CMBlockBufferReplaceDataBytes(avcc.as_ptr().cast(), block_buffer.raw, 0, avcc.len())
            };
            if status != 0 {
                bail!("CMBlockBufferReplaceDataBytes failed with OSStatus {status}");
            }

            let mut sample_buffer: CMSampleBufferRef = ptr::null_mut();
            let sample_size = avcc.len();
            let status = unsafe {
                CMSampleBufferCreateReady(
                    ptr::null_mut(),
                    block_buffer.raw,
                    self._format_desc.raw,
                    1,
                    0,
                    ptr::null(),
                    1,
                    &sample_size,
                    &mut sample_buffer,
                )
            };
            if status != 0 || sample_buffer.is_null() {
                bail!("CMSampleBufferCreateReady failed with OSStatus {status}");
            }
            let sample_buffer = CfGuard::new(sample_buffer);

            let mut info_flags = 0;
            let status = unsafe {
                VTDecompressionSessionDecodeFrame(
                    self.session,
                    sample_buffer.raw,
                    0,
                    ptr::null_mut(),
                    &mut info_flags,
                )
            };
            if status != 0 {
                bail!("VTDecompressionSessionDecodeFrame failed with OSStatus {status}");
            }
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            let converted = avcc_to_annex_b(avcc, self._format_desc.nal_length_size);
            if converted.is_empty() {
                return Ok(());
            }
            let mut annex_b = if self.needs_params {
                self.needs_params = false;
                let mut combined =
                    Vec::with_capacity(self._format_desc.annex_b_params.len() + converted.len());
                combined.extend_from_slice(&self._format_desc.annex_b_params);
                combined.extend_from_slice(&converted);
                combined
            } else {
                converted
            };
            match &mut self.backend {
                LinuxVideoBackend::LibAv { packet_tx, .. } => {
                    if let Some(tx) = packet_tx {
                        if let Err(error) = tx.try_send(annex_b) {
                            tracing::debug!(%error, "Dropping video access unit because decoder queue is full");
                        }
                    }
                }
                LinuxVideoBackend::Ffmpeg { stdin, .. } => {
                    // Append an Access Unit Delimiter + zero-byte Annex-B padding so
                    // ffmpeg's parser and AVIOContext emit each frame immediately.
                    match self._format_desc.codec {
                        VideoCodec::Hevc => {
                            annex_b.extend_from_slice(&[0, 0, 0, 1, 0x46, 0x01, 0x50])
                        }
                        VideoCodec::H264 => annex_b.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]),
                    }
                    annex_b.resize(annex_b.len() + 2048, 0);
                    if let Some(sin) = stdin.as_mut() {
                        sin.write_all(&annex_b)?;
                        sin.flush()?;
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn avcc_to_annex_b(avcc: &[u8], nal_length_size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    let mut offset = 0;
    let n = nal_length_size.clamp(1, 4);
    while offset + n <= avcc.len() {
        let mut nal_len = 0_usize;
        for &b in &avcc[offset..offset + n] {
            nal_len = (nal_len << 8) | (b as usize);
        }
        offset += n;
        if nal_len == 0 || offset + nal_len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[offset..offset + nal_len]);
        offset += nal_len;
    }
    out
}

fn annex_b_payload(nal: &[u8]) -> Result<&[u8]> {
    let payload = if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        bail!("Expected an Annex-B NAL start code");
    };
    if payload.len() < 2 {
        bail!("NAL is missing its two-byte header");
    }
    Ok(payload)
}

#[cfg(target_os = "macos")]
struct CfGuard<T> {
    raw: *mut T,
}

#[cfg(target_os = "macos")]
impl<T> CfGuard<T> {
    fn new(raw: *mut T) -> Self {
        Self { raw }
    }
}

#[cfg(target_os = "macos")]
impl<T> Drop for CfGuard<T> {
    fn drop(&mut self) {
        unsafe { CFRelease(self.raw.cast()) }
    }
}

impl Drop for HardwareDecoder {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            if !self.session.is_null() {
                let _ = VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session.cast());
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            match &mut self.backend {
                LinuxVideoBackend::LibAv { packet_tx, worker } => {
                    drop(packet_tx.take());
                    if let Some(handle) = worker.take() {
                        let _ = handle.join();
                    }
                }
                LinuxVideoBackend::Ffmpeg {
                    stdin,
                    child,
                    stop,
                    reader_thread,
                } => {
                    stop.store(true, Ordering::Release);
                    drop(stdin.take());
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(handle) = reader_thread.take() {
                        let _ = handle.join();
                    }
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) mod libav_sys {
    use anyhow::{bail, Result};
    use std::{
        ffi::{c_char, c_int, c_void, CStr},
        ptr,
    };

    const RTLD_LAZY: c_int = 0x00001;

    pub const AV_CODEC_FLAG_LOW_DELAY: i32 = 1 << 19;
    pub const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;

    pub const AVCTX_FLAGS_OFFSET: usize = 64;
    pub const AVCTX_EXTRADATA_OFFSET: usize = 72;
    pub const AVCTX_EXTRADATA_SIZE_OFFSET: usize = 80;

    pub const AVPKT_DATA_OFFSET: usize = 24;
    pub const AVPKT_SIZE_OFFSET: usize = 32;

    pub const AVFRAME_DATA_OFFSET: usize = 0;
    pub const AVFRAME_LINESIZE_OFFSET: usize = 64;
    pub const AVFRAME_WIDTH_OFFSET: usize = 104;
    pub const AVFRAME_HEIGHT_OFFSET: usize = 108;
    pub const AVFRAME_NB_SAMPLES_OFFSET: usize = 112;
    pub const AVFRAME_FORMAT_OFFSET: usize = 116;

    #[cfg(target_os = "linux")]
    extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
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

    unsafe fn load_any(candidates: &[&[u8]]) -> Result<*mut c_void> {
        for name in candidates {
            let h = dlopen(name.as_ptr().cast(), RTLD_LAZY);
            if !h.is_null() {
                return Ok(h);
            }
        }
        bail!("Could not load shared library")
    }

    unsafe fn load_sym<T>(handle: *mut c_void, name: &CStr) -> Result<T> {
        let ptr = dlsym(handle, name.as_ptr());
        if ptr.is_null() {
            bail!("Missing symbol {}", name.to_string_lossy());
        }
        Ok(std::mem::transmute_copy(&ptr))
    }

    /// Runtime-loaded `libavcodec` + `libavutil` dispatch table and owned decoder structs.
    pub struct LibAvDecoder {
        avcodec_lib: *mut c_void,
        avutil_lib: *mut c_void,
        pub ctx: *mut u8,
        pub pkt: *mut u8,
        pub frame: *mut u8,
        avcodec_free_context: unsafe extern "C" fn(*mut *mut u8),
        av_packet_free: unsafe extern "C" fn(*mut *mut u8),
        av_frame_free: unsafe extern "C" fn(*mut *mut u8),
        av_frame_unref: unsafe extern "C" fn(*mut u8),
        avcodec_send_packet: unsafe extern "C" fn(*mut u8, *const u8) -> c_int,
        avcodec_receive_frame: unsafe extern "C" fn(*mut u8, *mut u8) -> c_int,
    }

    unsafe impl Send for LibAvDecoder {}

    impl LibAvDecoder {
        pub fn open(
            codec_name: &CStr,
            low_delay: bool,
            extradata: Option<&[u8]>,
            options: &[(&CStr, &CStr)],
        ) -> Result<Self> {
            unsafe {
                let avcodec_lib = load_any(&[
                    b"libavcodec.so.63\0",
                    b"libavcodec.so.62\0",
                    b"libavcodec.so.61\0",
                    b"libavcodec.so.60\0",
                    b"libavcodec.so.59\0",
                    b"libavcodec.so.58\0",
                    b"libavcodec.so\0",
                ])?;
                let avutil_lib = match load_any(&[
                    b"libavutil.so.61\0",
                    b"libavutil.so.60\0",
                    b"libavutil.so.59\0",
                    b"libavutil.so.58\0",
                    b"libavutil.so.57\0",
                    b"libavutil.so.56\0",
                    b"libavutil.so\0",
                ]) {
                    Ok(h) => h,
                    Err(e) => {
                        dlclose(avcodec_lib);
                        return Err(e);
                    }
                };

                let find_decoder_by_name: unsafe extern "C" fn(*const c_char) -> *const c_void =
                    load_sym(avcodec_lib, c"avcodec_find_decoder_by_name")?;
                let alloc_context3: unsafe extern "C" fn(*const c_void) -> *mut u8 =
                    load_sym(avcodec_lib, c"avcodec_alloc_context3")?;
                let open2: unsafe extern "C" fn(*mut u8, *const c_void, *mut *mut c_void) -> c_int =
                    load_sym(avcodec_lib, c"avcodec_open2")?;
                let avcodec_free_context: unsafe extern "C" fn(*mut *mut u8) =
                    load_sym(avcodec_lib, c"avcodec_free_context")?;
                let av_packet_alloc: unsafe extern "C" fn() -> *mut u8 =
                    load_sym(avcodec_lib, c"av_packet_alloc")?;
                let av_packet_free: unsafe extern "C" fn(*mut *mut u8) =
                    load_sym(avcodec_lib, c"av_packet_free")?;
                let avcodec_send_packet: unsafe extern "C" fn(*mut u8, *const u8) -> c_int =
                    load_sym(avcodec_lib, c"avcodec_send_packet")?;
                let avcodec_receive_frame: unsafe extern "C" fn(*mut u8, *mut u8) -> c_int =
                    load_sym(avcodec_lib, c"avcodec_receive_frame")?;

                let av_frame_alloc: unsafe extern "C" fn() -> *mut u8 =
                    load_sym(avutil_lib, c"av_frame_alloc")?;
                let av_frame_free: unsafe extern "C" fn(*mut *mut u8) =
                    load_sym(avutil_lib, c"av_frame_free")?;
                let av_frame_unref: unsafe extern "C" fn(*mut u8) =
                    load_sym(avutil_lib, c"av_frame_unref")?;
                let av_dict_set: unsafe extern "C" fn(
                    *mut *mut c_void,
                    *const c_char,
                    *const c_char,
                    c_int,
                ) -> c_int = load_sym(avutil_lib, c"av_dict_set")?;
                let av_dict_free: unsafe extern "C" fn(*mut *mut c_void) =
                    load_sym(avutil_lib, c"av_dict_free")?;
                let av_mallocz: unsafe extern "C" fn(usize) -> *mut u8 =
                    load_sym(avutil_lib, c"av_mallocz")?;

                let codec = find_decoder_by_name(codec_name.as_ptr());
                if codec.is_null() {
                    dlclose(avutil_lib);
                    dlclose(avcodec_lib);
                    bail!(
                        "avcodec_find_decoder_by_name returned null for {}",
                        codec_name.to_string_lossy()
                    );
                }

                let mut ctx = alloc_context3(codec);
                let mut pkt = av_packet_alloc();
                let mut frame = av_frame_alloc();
                if ctx.is_null() || pkt.is_null() || frame.is_null() {
                    if !frame.is_null() {
                        av_frame_free(&mut frame);
                    }
                    if !pkt.is_null() {
                        av_packet_free(&mut pkt);
                    }
                    if !ctx.is_null() {
                        avcodec_free_context(&mut ctx);
                    }
                    dlclose(avutil_lib);
                    dlclose(avcodec_lib);
                    bail!("Failed to allocate libavcodec context/packet/frame");
                }

                if low_delay {
                    let flags_ptr = ctx.add(AVCTX_FLAGS_OFFSET).cast::<i32>();
                    let cur = flags_ptr.read_unaligned();
                    flags_ptr.write_unaligned(cur | AV_CODEC_FLAG_LOW_DELAY);
                }

                if let Some(extra) = extradata {
                    if !extra.is_empty() {
                        let buf = av_mallocz(extra.len() + AV_INPUT_BUFFER_PADDING_SIZE);
                        if buf.is_null() {
                            av_frame_free(&mut frame);
                            av_packet_free(&mut pkt);
                            avcodec_free_context(&mut ctx);
                            dlclose(avutil_lib);
                            dlclose(avcodec_lib);
                            bail!("av_mallocz failed for extradata");
                        }
                        ptr::copy_nonoverlapping(extra.as_ptr(), buf, extra.len());
                        ctx.add(AVCTX_EXTRADATA_OFFSET)
                            .cast::<*mut u8>()
                            .write_unaligned(buf);
                        ctx.add(AVCTX_EXTRADATA_SIZE_OFFSET)
                            .cast::<i32>()
                            .write_unaligned(extra.len() as i32);
                    }
                }

                let mut opts: *mut c_void = ptr::null_mut();
                for &(k, v) in options {
                    av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
                }
                let rc = open2(ctx, codec, &mut opts);
                av_dict_free(&mut opts);
                if rc < 0 {
                    av_frame_free(&mut frame);
                    av_packet_free(&mut pkt);
                    avcodec_free_context(&mut ctx);
                    dlclose(avutil_lib);
                    dlclose(avcodec_lib);
                    bail!("avcodec_open2 failed with code {rc}");
                }

                Ok(Self {
                    avcodec_lib,
                    avutil_lib,
                    ctx,
                    pkt,
                    frame,
                    avcodec_free_context,
                    av_packet_free,
                    av_frame_free,
                    av_frame_unref,
                    avcodec_send_packet,
                    avcodec_receive_frame,
                })
            }
        }

        pub fn send_packet_and_drain<F>(&mut self, payload: &[u8], mut on_frame: F) -> i32
        where
            F: FnMut(*const u8),
        {
            // FFmpeg requires AVPacket data to be zero-padded by AV_INPUT_BUFFER_PADDING_SIZE.
            let mut padded = Vec::with_capacity(payload.len() + AV_INPUT_BUFFER_PADDING_SIZE);
            padded.extend_from_slice(payload);
            padded.resize(payload.len() + AV_INPUT_BUFFER_PADDING_SIZE, 0);

            unsafe {
                self.pkt
                    .add(AVPKT_DATA_OFFSET)
                    .cast::<*mut u8>()
                    .write_unaligned(padded.as_mut_ptr());
                self.pkt
                    .add(AVPKT_SIZE_OFFSET)
                    .cast::<i32>()
                    .write_unaligned(payload.len() as i32);

                let send_rc = (self.avcodec_send_packet)(self.ctx, self.pkt);

                self.pkt
                    .add(AVPKT_DATA_OFFSET)
                    .cast::<*mut u8>()
                    .write_unaligned(ptr::null_mut());
                self.pkt
                    .add(AVPKT_SIZE_OFFSET)
                    .cast::<i32>()
                    .write_unaligned(0);

                if send_rc >= 0 {
                    while (self.avcodec_receive_frame)(self.ctx, self.frame) == 0 {
                        on_frame(self.frame);
                        (self.av_frame_unref)(self.frame);
                    }
                }
                send_rc
            }
        }
    }

    impl Drop for LibAvDecoder {
        fn drop(&mut self) {
            unsafe {
                if !self.frame.is_null() {
                    (self.av_frame_free)(&mut self.frame);
                }
                if !self.pkt.is_null() {
                    (self.av_packet_free)(&mut self.pkt);
                }
                if !self.ctx.is_null() {
                    (self.avcodec_free_context)(&mut self.ctx);
                }
                if !self.avutil_lib.is_null() {
                    dlclose(self.avutil_lib);
                }
                if !self.avcodec_lib.is_null() {
                    dlclose(self.avcodec_lib);
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod libav_video {
    use super::libav_sys::*;
    use crate::codec::format_desc::VideoCodec;
    use anyhow::Result;
    use std::ptr;

    const AV_PIX_FMT_YUV420P: i32 = 0;
    const AV_PIX_FMT_YUVJ420P: i32 = 12;
    const AV_PIX_FMT_NV12: i32 = 23;

    pub struct LibAvVideoContext {
        dec: LibAvDecoder,
        warned_unexpected_fmt: bool,
    }

    impl LibAvVideoContext {
        pub fn new(codec: VideoCodec) -> Result<Self> {
            let codec_name = match codec {
                VideoCodec::Hevc => c"hevc",
                VideoCodec::H264 => c"h264",
            };
            let dec = LibAvDecoder::open(
                codec_name,
                true,
                None,
                &[
                    (c"threads", c"4"),
                    (c"thread_type", c"slice"),
                    (c"flags", c"low_delay"),
                ],
            )?;
            Ok(Self {
                dec,
                warned_unexpected_fmt: false,
            })
        }

        pub fn send_annex_b<F>(&mut self, annex_b: &[u8], mut on_frame: F)
        where
            F: FnMut(usize, usize, Vec<u8>, bool, usize),
        {
            let mut warned = self.warned_unexpected_fmt;
            let mut count = 0usize;
            self.dec.send_packet_and_drain(annex_b, |frame| unsafe {
                let w = frame
                    .add(AVFRAME_WIDTH_OFFSET)
                    .cast::<i32>()
                    .read_unaligned();
                let h = frame
                    .add(AVFRAME_HEIGHT_OFFSET)
                    .cast::<i32>()
                    .read_unaligned();
                let pix_fmt = frame
                    .add(AVFRAME_FORMAT_OFFSET)
                    .cast::<i32>()
                    .read_unaligned();
                if w <= 0 || h <= 0 {
                    return;
                }
                let width = w as usize;
                let height = h as usize;
                let data_ptrs = frame
                    .add(AVFRAME_DATA_OFFSET)
                    .cast::<[*const u8; 3]>()
                    .read_unaligned();
                let linesizes = frame
                    .add(AVFRAME_LINESIZE_OFFSET)
                    .cast::<[i32; 3]>()
                    .read_unaligned();

                if let Some(nv12) = frame_to_nv12(width, height, pix_fmt, data_ptrs, linesizes) {
                    let full_range = pix_fmt == AV_PIX_FMT_YUVJ420P;
                    count += 1;
                    on_frame(width, height, nv12, full_range, count);
                } else if !warned {
                    warned = true;
                    tracing::warn!(pix_fmt, "Unsupported AVFrame pixel format from libavcodec");
                }
            });
            self.warned_unexpected_fmt = warned;
        }
    }

    unsafe fn frame_to_nv12(
        width: usize,
        height: usize,
        pix_fmt: i32,
        data: [*const u8; 3],
        linesize: [i32; 3],
    ) -> Option<Vec<u8>> {
        let y_src = data[0];
        if y_src.is_null() {
            return None;
        }
        let uv_h = height / 2;
        let uv_w = width / 2;
        let mut nv12 = vec![0_u8; width * height * 3 / 2];
        let y_stride = linesize[0] as isize;

        for row in 0..height {
            let src = y_src.offset(row as isize * y_stride);
            let dst = &mut nv12[row * width..(row + 1) * width];
            ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), width);
        }

        let uv_dst_base = width * height;
        match pix_fmt {
            AV_PIX_FMT_NV12 => {
                let uv_src = data[1];
                if uv_src.is_null() {
                    return None;
                }
                let uv_stride = linesize[1] as isize;
                for row in 0..uv_h {
                    let src = uv_src.offset(row as isize * uv_stride);
                    let dst = &mut nv12[uv_dst_base + row * width..uv_dst_base + (row + 1) * width];
                    ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), uv_w * 2);
                }
                Some(nv12)
            }
            AV_PIX_FMT_YUV420P | AV_PIX_FMT_YUVJ420P => {
                let u_src = data[1];
                let v_src = data[2];
                if u_src.is_null() || v_src.is_null() {
                    return None;
                }
                let u_stride = linesize[1] as isize;
                let v_stride = linesize[2] as isize;
                for row in 0..uv_h {
                    let u_row =
                        std::slice::from_raw_parts(u_src.offset(row as isize * u_stride), uv_w);
                    let v_row =
                        std::slice::from_raw_parts(v_src.offset(row as isize * v_stride), uv_w);
                    let dst = &mut nv12[uv_dst_base + row * width..uv_dst_base + (row + 1) * width];
                    for col in 0..uv_w {
                        dst[col * 2] = u_row[col];
                        dst[col * 2 + 1] = v_row[col];
                    }
                }
                Some(nv12)
            }
            _ => None,
        }
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::libav_video::LibAvVideoContext;
    use crate::codec::format_desc::VideoCodec;

    #[test]
    fn libav_video_decoder_initializes_for_hevc_and_h264() {
        let _hevc = LibAvVideoContext::new(VideoCodec::Hevc)
            .expect("libavcodec HEVC decoder should initialize on Linux");
        let _h264 = LibAvVideoContext::new(VideoCodec::H264)
            .expect("libavcodec H.264 decoder should initialize on Linux");
    }
}

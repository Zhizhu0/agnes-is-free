//! Inline video player using Windows Media Foundation (IMFSourceReader).
//!
//! Decodes MP4/H.264 videos to RGBA frames on a background thread and pushes
//! them to the UI via an mpsc channel. The UI thread uploads each received
//! frame as an egui TextureHandle.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ── Public types ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct FrameData {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts: Duration,
}

#[derive(Debug, Clone)]
pub enum PlayerCommand {
    Pause,
    Resume,
    Stop,
    Seek(Duration),
}

pub struct SharedState {
    pub playing: Mutex<bool>,
    pub current_pts: Mutex<Duration>,
    pub total_duration: Mutex<Option<Duration>>,
}

pub struct VideoPlayerState {
    pub resource_id: String,
    file_path: String,
    inner: Option<PlayerThread>,
    pub shared: Arc<SharedState>,
    /// Whether the decoder has produced at least one frame.
    /// Used to distinguish "still initializing" from "finished playback".
    has_frame: bool,
}

struct PlayerThread {
    cmd_tx: mpsc::Sender<PlayerCommand>,
    frame_rx: mpsc::Receiver<Result<FrameData, String>>,
    _handle: thread::JoinHandle<()>,
}

// ── Construction & control ────────────────────────────────────────────────

impl VideoPlayerState {
    pub fn new(resource_id: String, file_path: String) -> Self {
        Self {
            resource_id,
            file_path,
            inner: None,
            shared: Arc::new(SharedState {
                playing: Mutex::new(false),
                current_pts: Mutex::new(Duration::ZERO),
                total_duration: Mutex::new(None),
            }),
            has_frame: false,
        }
    }

    pub fn start(&mut self) {
        if self.inner.is_some() {
            return;
        }
        let file_path = self.file_path.clone();
        let shared = self.shared.clone();
        let rid = self.resource_id.clone();

        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>();
        let (frame_tx, frame_rx) = mpsc::channel::<Result<FrameData, String>>();

        let handle = thread::Builder::new()
            .name(format!("video-decoder-{rid}"))
            .spawn(move || {
                run_decoder(&file_path, cmd_rx, frame_tx, shared);
            })
            .expect("spawn decoder");

        self.inner = Some(PlayerThread { cmd_tx, frame_rx, _handle: handle });
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.shared.playing.lock().ok().as_deref(), Some(true))
    }

    fn send_cmd(&self, cmd: PlayerCommand) -> Result<(), String> {
        match &self.inner {
            Some(i) => i.cmd_tx.send(cmd).map_err(|e| format!("{e}")),
            None => Err("not started".into()),
        }
    }

    pub fn pause(&self)   { let _ = self.send_cmd(PlayerCommand::Pause); }
    pub fn resume(&self)  { let _ = self.send_cmd(PlayerCommand::Resume); }
    pub fn seek(&self, p: Duration) { let _ = self.send_cmd(PlayerCommand::Seek(p)); }

    pub fn scheduled_stop(&mut self) {
        let _ = self.send_cmd(PlayerCommand::Stop);
        self.inner = None;
    }

    pub fn poll_frames(&mut self) -> Option<FrameData> {
        let inner = self.inner.as_ref()?;
        let mut latest = None;
        loop {
            match inner.frame_rx.try_recv() {
                Ok(Ok(f)) => {
                    if let Ok(mut p) = self.shared.current_pts.lock() { *p = f.pts; }
                    self.has_frame = true;
                    latest = Some(f);
                }
                Ok(Err(e)) => {
                    eprintln!("[video_player] decode error for {}: {}", self.resource_id, e);
                    if let Ok(mut p) = self.shared.playing.lock() { *p = false; }
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Ok(mut p) = self.shared.playing.lock() { *p = false; }
                    break;
                }
            }
        }
        latest
    }

    /// Returns true once the decoder has produced at least one frame.
    pub fn has_frame(&self) -> bool {
        self.has_frame
    }
}

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}.{}", s / 60, s % 60, d.subsec_millis() / 100)
}

#[cfg(windows)]
fn run_decoder(
    file_path: &str,
    cmd_rx: mpsc::Receiver<PlayerCommand>,
    frame_tx: mpsc::Sender<Result<FrameData, String>>,
    shared: Arc<SharedState>,
) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        wmf_decode(file_path, &cmd_rx, &frame_tx, &shared);
    }));
    if r.is_err() {
        let _ = frame_tx.send(Err("decoder panicked".into()));
    }
}

#[cfg(windows)]
unsafe fn wmf_decode(
    file_path: &str,
    cmd_rx: &mpsc::Receiver<PlayerCommand>,
    frame_tx: &mpsc::Sender<Result<FrameData, String>>,
    shared: &Arc<SharedState>,
) {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

    // Init COM (apartment-threaded) + MF
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);

    // Build null-terminated wide path
    let wide: Vec<u16> = Path::new(file_path)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Create the source reader — enable video processing so the decoder
    // can convert to RGB formats via the video processor MFT.
    let reader = {
        let mut attrs: Option<IMFAttributes> = None;
        if MFCreateAttributes(&mut attrs, 0).is_err() {
            let _ = frame_tx.send(Err("MFCreateAttributes failed".into()));
            *shared.playing.lock().unwrap() = false;
            return;
        }
        let attrs = attrs.unwrap();
        // MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING = 0x10000005 (UINT32 value=1)
        let _ = attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);
        match MFCreateSourceReaderFromURL(
            windows::core::PCWSTR(wide.as_ptr()),
            &attrs,
        ) {
            Ok(r) => r,
            Err(e) => {
                let _ = frame_tx.send(Err(format!("MFCreateSourceReaderFromURL failed: {e}")));
                *shared.playing.lock().unwrap() = false;
                return;
            }
        }
    };

    // Pixel format candidates — preferred order for decoder compatibility
    const CANDIDATES: [windows::core::GUID; 8] = [
        MFVideoFormat_NV12,
        MFVideoFormat_YUY2,
        MFVideoFormat_RGB32,
        MFVideoFormat_ARGB32,
        MFVideoFormat_I420,
        MFVideoFormat_YV12,
        MFVideoFormat_RGB24,
        MFVideoFormat_AYUV,
    ];

    // Disable streams other than the first video stream; negotiate pixel format
    let mut selected_format: u8 = 0;
    {
        // Select first video stream, disable others
        let mut idx: u32 = 0;
        loop {
            let mt = match reader.GetNativeMediaType(idx, 0) {
                Ok(m) => m,
                Err(_) => break,
            };
            let major = mt.GetGUID(&MF_MT_MAJOR_TYPE).unwrap_or_default();
            let enabled = major == MFMediaType_Video;
            let stream_idx = if enabled { MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32 } else { idx };
            let _ = reader.SetStreamSelection(stream_idx, enabled);
            idx += 1;
        }

        // Get native frame size to include in output media type
        let (native_w, native_h): (u32, u32) = {
            let mut w = 0u32;
            let mut h = 0u32;
            for i in 0..=255 {
                if let Ok(mt) = reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, i) {
                    if mt.GetGUID(&MF_MT_MAJOR_TYPE).unwrap_or_default() == MFMediaType_Video {
                        let size: u64 = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                        w = (size >> 32) as u32;
                        h = (size & 0xFFFF_FFFF) as u32;
                        if w > 0 && h > 0 { break; }
                    }
                }
            }
            (w, h)
        };

        // Try each candidate; decoders are picky about which output formats they accept
        let mut fmt_err = None;
        for (i, subtype) in CANDIDATES.iter().enumerate() {
            let out_mt = match MFCreateMediaType() {
                Ok(m) => m,
                Err(e) => { fmt_err = Some(format!("MFCreateMediaType failed: {e}")); break; }
            };
            let _ = out_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
            let _ = out_mt.SetGUID(&MF_MT_SUBTYPE, &*subtype);
            if native_w > 0 && native_h > 0 {
                let _ = out_mt.SetUINT64(&MF_MT_FRAME_SIZE, ((native_w as u64) << 32) | (native_h as u64));
            }
            let _ = out_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            let hr = reader.SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &out_mt,
            );
            if hr.is_ok() {
                selected_format = match i {
                    0 => 10,  // NV12
                    1 => 1,   // YUY2
                    2 => 0,   // RGB32/BGRA
                    3 => 0,   // ARGB32 (no swap needed, but we treat like BGRA)
                    4 => 11,  // I420
                    5 => 12,  // YV12
                    6 => 24,  // RGB24
                    _ => 0,   // AYUV → treat as raw
                };
                fmt_err = None;
                eprintln!("[video_player] using pixel format index {i}");
                break;
            }
            fmt_err = Some(format!("SetCurrentMediaType({i}/{:?}) failed: {hr:?}", subtype));
        }

        if let Some(e) = fmt_err {
            let _ = frame_tx.send(Err(format!("no supported pixel format: {e}")));
            *shared.playing.lock().unwrap() = false;
            return;
        }
    }

    // Query frame size (MF_MT_FRAME_SIZE packed as u64 = (width << 32) | height)
    let (width, height): (u32, u32) = {
        let mt = match reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) {
            Ok(m) => m,
            Err(e) => {
                let _ = frame_tx.send(Err(format!("GetCurrentMediaType failed: {e}")));
                return;
            }
        };
        let size_packed: u64 = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let w = (size_packed >> 32) as u32;
        let h = (size_packed & 0xFFFF_FFFF) as u32;
        if w == 0 || h == 0 {
            let _ = frame_tx.send(Err("invalid dimensions".into()));
            return;
        }
        (w, h)
    };

    let rgba_bytes = (width * height * 4) as usize;

    *shared.playing.lock().unwrap() = true;

    // Push a placeholder frame so UI can allocate texture immediately
    let placeholder = FrameData {
        rgba: vec![0x22; rgba_bytes],
        width,
        height,
        pts: Duration::ZERO,
    };
    if frame_tx.send(Ok(placeholder)).is_err() { return; }
    let mut playing = true;
    let mut last_flip = std::time::Instant::now();
    let frame_interval = Duration::from_millis(40);

    'main: loop {
        // Drain commands
        loop {
            match cmd_rx.try_recv() {
                Ok(PlayerCommand::Pause) => {
                    playing = false;
                    *shared.playing.lock().unwrap() = false;
                }
                Ok(PlayerCommand::Resume) => {
                    playing = true;
                    *shared.playing.lock().unwrap() = true;
                    last_flip = std::time::Instant::now();
                }
                Ok(PlayerCommand::Stop) => {
                    *shared.playing.lock().unwrap() = false;
                    break 'main;
                }
                Ok(PlayerCommand::Seek(_pos)) => {
                    // WMF seek via SetCurrentPosition is finicky;
                    // for now just flag — we'd need a full IMFSource Seek for proper seeking
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'main,
            }
        }

        if !playing {
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        // Frame-rate gate
        let elapsed = last_flip.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval.saturating_sub(elapsed));
        }

        let mut sample: Option<IMFSample> = None;
        let mut flags: u32 = 0;
        let mut timestamp: i64 = 0;
        let mut actual_index: u32 = 0;

        if let Err(_e) = reader.ReadSample(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            0,
            Some(&mut actual_index),
            Some(&mut flags),
            Some(&mut timestamp),
            Some(&mut sample),
        ) {
            // likely EOF
            break 'main;
        }

        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            *shared.playing.lock().unwrap() = false;
            break 'main;
        }

        if let Some(s) = sample {
            let buffer = match s.ConvertToContiguousBuffer() {
                Ok(b) => b,
                Err(_) => continue,
            };

            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len: u32 = 0;
            let mut cur_len: u32 = 0;
            if buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len)).is_ok() {
                let len = cur_len as usize;
                let data = std::slice::from_raw_parts(ptr, len);
                let rgba = decode_frame(data, selected_format, width, height, rgba_bytes);
                let _ = buffer.Unlock();

                let pts = Duration::from_secs_f64(timestamp as f64 / 10_000_000.0);

                if frame_tx.send(Ok(FrameData { rgba, width, height, pts })).is_err() {
                    break 'main;
                }
                last_flip = std::time::Instant::now();
            }
        }
    }

    let _ = MFShutdown();
    CoUninitialize();
}

/// Convert a locked IMFMediaBuffer into an RGBA frame.
fn decode_frame(
    src: &[u8],
    format: u8,
    w: u32,
    h: u32,
    rgba_bytes: usize,
) -> Vec<u8> {
    let mut rgba = vec![0u8; rgba_bytes];
    let w_usize = w as usize;
    let h_usize = h as usize;
    match format {
        // RGB32/ARGB32 = BGRA in memory — swap R↔B channels
        0 => {
            let copy = ((w_usize * h_usize * 4)).min(src.len()).min(rgba_bytes);
            rgba[..copy].copy_from_slice(&src[..copy]);
            for px in rgba.chunks_exact_mut(4) { px.swap(0, 2); }
        }
        // YUY2 = 4:2:2 packed (Y0 U Y1 V per 2 pixels) — convert via BT.601
        1 => {
            for y in 0..h_usize {
                for x in (0..w_usize).step_by(2) {
                    let base = (y * w_usize + x) * 2;
                    if base + 3 >= src.len() { break; }
                    let y0 = src[base] as i32;
                    let u  = src[base + 1] as i32 - 128;
                    let y1 = src[base + 2] as i32;
                    let v  = src[base + 3] as i32 - 128;
                    let (r0, g0, b0) = yuv_to_rgb(y0, u, v);
                    let o0 = (y * w_usize + x) * 4;
                    if o0 + 3 < rgba_bytes { rgba[o0]=r0; rgba[o0+1]=g0; rgba[o0+2]=b0; rgba[o0+3]=255; }
                    if x + 1 < w_usize {
                        let (r1, g1, b1) = yuv_to_rgb(y1, u, v);
                        let o1 = (y * w_usize + x + 1) * 4;
                        if o1 + 3 < rgba_bytes { rgba[o1]=r1; rgba[o1+1]=g1; rgba[o1+2]=b1; rgba[o1+3]=255; }
                    }
                }
            }
        }
        // NV12 = 4:2:0 planar (Y plane + interleaved UV plane)
        10 => {
            let y_plane_size = w_usize * h_usize;
            for y in 0..h_usize {
                for x in 0..w_usize {
                    let y_idx = y * w_usize + x;
                    if y_idx >= src.len() { break; }
                    let y_val = src[y_idx] as i32;
                    let chroma_y = y / 2;
                    let chroma_x = (x / 2) * 2;
                    let uv_base = y_plane_size + chroma_y * w_usize + chroma_x;
                    let u = if uv_base < src.len() { src[uv_base] as i32 - 128 } else { 0 };
                    let v = if uv_base + 1 < src.len() { src[uv_base + 1] as i32 - 128 } else { 0 };
                    let (r, g, b) = yuv_to_rgb(y_val, u, v);
                    let o = (y * w_usize + x) * 4;
                    if o + 3 < rgba_bytes { rgba[o]=r; rgba[o+1]=g; rgba[o+2]=b; rgba[o+3]=255; }
                }
            }
        }
        // I420 = 4:2:0 planar (Y plane + U plane + V plane, separate)
        11 => {
            let y_size = w_usize * h_usize;
            let c_size = (w_usize / 2) * (h_usize / 2);
            let u_offset = y_size;
            let v_offset = y_size + c_size;
            for y in 0..h_usize {
                for x in 0..w_usize {
                    let y_idx = y * w_usize + x;
                    if y_idx >= src.len() { break; }
                    let y_val = src[y_idx] as i32;
                    let cx = x / 2;
                    let cy = y / 2;
                    let c_idx = cy * (w_usize / 2) + cx;
                    let u = if u_offset + c_idx < src.len() { src[u_offset + c_idx] as i32 - 128 } else { 0 };
                    let v = if v_offset + c_idx < src.len() { src[v_offset + c_idx] as i32 - 128 } else { 0 };
                    let (r, g, b) = yuv_to_rgb(y_val, u, v);
                    let o = (y * w_usize + x) * 4;
                    if o + 3 < rgba_bytes { rgba[o]=r; rgba[o+1]=g; rgba[o+2]=b; rgba[o+3]=255; }
                }
            }
        }
        // YV12 = 4:2:0 planar (Y plane + V plane + U plane, separate, V before U)
        12 => {
            let y_size = w_usize * h_usize;
            let c_size = (w_usize / 2) * (h_usize / 2);
            let v_offset = y_size;
            let u_offset = y_size + c_size;
            for y in 0..h_usize {
                for x in 0..w_usize {
                    let y_idx = y * w_usize + x;
                    if y_idx >= src.len() { break; }
                    let y_val = src[y_idx] as i32;
                    let cx = x / 2;
                    let cy = y / 2;
                    let c_idx = cy * (w_usize / 2) + cx;
                    let u = if u_offset + c_idx < src.len() { src[u_offset + c_idx] as i32 - 128 } else { 0 };
                    let v = if v_offset + c_idx < src.len() { src[v_offset + c_idx] as i32 - 128 } else { 0 };
                    let (r, g, b) = yuv_to_rgb(y_val, u, v);
                    let o = (y * w_usize + x) * 4;
                    if o + 3 < rgba_bytes { rgba[o]=r; rgba[o+1]=g; rgba[o+2]=b; rgba[o+3]=255; }
                }
            }
        }
        // RGB24 = BGR bytes — expand to BGRA with alpha
        24 => {
            let pixels = (w_usize * h_usize).min(src.len() / 3);
            for i in 0..pixels {
                let si = i * 3;
                let di = i * 4;
                if di + 4 <= rgba_bytes && si + 3 <= src.len() {
                    rgba[di]     = src[si + 2]; // R
                    rgba[di + 1] = src[si + 1]; // G
                    rgba[di + 2] = src[si];     // B
                    rgba[di + 3] = 255;
                }
            }
        }
        // Fallback: treat as BGRA
        _ => {
            let copy = ((w_usize * h_usize * 4)).min(src.len()).min(rgba_bytes);
            rgba[..copy].copy_from_slice(&src[..copy]);
        }
    }
    rgba
}

/// BT.601 limited-range Y′UV → RGB conversion.
fn yuv_to_rgb(y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    let r = (y * 298 + v * 409 + 128) >> 8;
    let g = (y * 298 - u * 100 - v * 208 + 128) >> 8;
    let b = (y * 298 + u * 516 + 128) >> 8;
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

#[inline(always)]
fn clamp_u8(v: i32) -> u8 {
    if v < 0 { 0 } else if v > 255 { 255 } else { v as u8 }
}

/// Extract the first video frame as PNG bytes (blocking, runs on a worker thread).
/// Returns Ok(png_bytes) or Err(reason).
pub fn extract_first_frame_png(file_path: &str) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            wmf_first_frame(file_path)
        })) {
        Ok(result) => result,
        Err(_) => Err("first-frame extraction panicked".to_string()),
    }
    }
    #[cfg(not(windows))]
    {
        Err("first-frame extraction only on Windows".into())
    }
}

#[cfg(windows)]
unsafe fn wmf_first_frame(file_path: &str) -> Result<Vec<u8>, String> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);

    let wide: Vec<u16> = Path::new(file_path)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let reader = {
        let mut attrs: Option<IMFAttributes> = None;
        if MFCreateAttributes(&mut attrs, 0).is_err() {
            CoUninitialize();
            let _ = MFShutdown();
            return Err("MFCreateAttributes failed".into());
        }
        let attrs = attrs.unwrap();
        let _ = attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);
        match MFCreateSourceReaderFromURL(windows::core::PCWSTR(wide.as_ptr()), &attrs) {
            Ok(r) => r,
            Err(e) => {
                CoUninitialize();
                let _ = MFShutdown();
                return Err(format!("MFCreateSourceReaderFromURL failed: {e}"));
            }
        }
    };

    // Disable non-video streams
    let mut idx: u32 = 0;
    loop {
        let mt = match reader.GetNativeMediaType(idx, 0) {
            Ok(m) => m,
            Err(_) => break,
        };
        let major = mt.GetGUID(&MF_MT_MAJOR_TYPE).unwrap_or_default();
        let enabled = major == MFMediaType_Video;
        let stream_idx = if enabled { MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32 } else { idx };
        let _ = reader.SetStreamSelection(stream_idx, enabled);
        idx += 1;
    }

    // Negotiate pixel format
    const CANDIDATES: [windows::core::GUID; 4] = [
        MFVideoFormat_RGB32,
        MFVideoFormat_NV12,
        MFVideoFormat_YUY2,
        MFVideoFormat_I420,
    ];
    let mut fmt_code: u8 = 0;
    let mut fmt_ok = false;
    for (i, subtype) in CANDIDATES.iter().enumerate() {
        let out_mt = match MFCreateMediaType() {
            Ok(m) => m,
            Err(_) => break,
        };
        let _ = out_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video);
        let _ = out_mt.SetGUID(&MF_MT_SUBTYPE, &*subtype);
        let _ = out_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
        if reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &out_mt).is_ok() {
            fmt_code = match i {
                0 => 0,   // RGB32/BGRA
                1 => 10,  // NV12
                2 => 1,   // YUY2
                _ => 11,  // I420
            };
            fmt_ok = true;
            break;
        }
    }
    if !fmt_ok {
        CoUninitialize();
        let _ = MFShutdown();
        return Err("no supported pixel format for first frame".into());
    }

    // Read first sample
    let mut sample: Option<IMFSample> = None;
    let mut flags: u32 = 0;
    let mut _timestamp: i64 = 0;
    reader
        .ReadSample(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            0,
            None,
            Some(&mut flags),
            Some(&mut _timestamp),
            Some(&mut sample),
        )
        .map_err(|e| format!("ReadSample failed: {e}"))?;

    let sample = sample.ok_or("no first sample")?;
    let buffer = sample.ConvertToContiguousBuffer().map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;

    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut _max_len: u32 = 0;
    let mut cur_len: u32 = 0;
    buffer.Lock(&mut ptr, Some(&mut _max_len), Some(&mut cur_len)).map_err(|e| format!("Lock: {e}"))?;
    let data = std::slice::from_raw_parts(ptr, cur_len as usize);

    // Get dimensions
    let mt = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
        .map_err(|e| format!("GetCurrentMediaType: {e}"))?;
    let packed: u64 = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
    let w = (packed >> 32) as u32;
    let h = (packed & 0xFFFF_FFFF) as u32;
    let _ = buffer.Unlock();

    if w == 0 || h == 0 {
        CoUninitialize();
        let _ = MFShutdown();
        return Err("invalid dimensions".into());
    }

    // Convert to RGBA
    let rgba_bytes = (w * h * 4) as usize;
    let rgba = decode_frame(data, fmt_code, w, h, rgba_bytes);

    // Flip vertically (WMF gives bottom-up RGB)
    let mut flipped = vec![0u8; rgba_bytes];
    let row_bytes = (w * 4) as usize;
    for y in 0..h as usize {
        let src_start = y * row_bytes;
        let dst_start = ((h as usize - 1) - y) * row_bytes;
        flipped[dst_start..dst_start + row_bytes].copy_from_slice(&rgba[src_start..src_start + row_bytes]);
    }

    // Encode as PNG via the `image` crate
    let png = {
        use image::ImageBuffer;
        let img: ImageBuffer<image::Rgba<u8>, _> = ImageBuffer::from_raw(w, h, flipped)
            .ok_or("failed to create image buffer")?;
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .map_err(|e| format!("png encode: {e}"))?;
        buf
    };

    CoUninitialize();
    let _ = MFShutdown();
    Ok(png)
}

#[cfg(not(windows))]
fn run_decoder(
    _file_path: &str,
    _cmd_rx: mpsc::Receiver<PlayerCommand>,
    frame_tx: mpsc::Sender<Result<FrameData, String>>,
    shared: Arc<SharedState>,
) {
    let _ = frame_tx.send(Err("inline video playback only on Windows".into()));
    shared.playing.lock().map(|mut p| *p = false).ok();
}

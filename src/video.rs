//! Videos, decoded by the `ffmpeg` command (FFmpeg is what every media
//! player sits on; using its command rather than a player like mpv, or
//! linking its libraries, keeps this small and free of FFmpeg version
//! ties). ffmpeg writes the frames raw to a pipe; a small thread reads each
//! frame, when asked, into memory the compositor reads: GPU buffers as
//! NV12 where it can (see `gpu`: no conversion to RGB, the compositor
//! scales them), else shared memory, cropped and scaled to exactly the
//! pixels the screen shows and converted to RGB by ffmpeg. Frames are
//! asked for at the video's rate and only while the compositor is drawing
//! the wallpaper, and ffmpeg waits on the full pipe meanwhile: covered,
//! a video costs nothing.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::config::Mode;
use crate::picture::{self, Fit};

/// Files shown as videos (by extension).
pub const EXTENSIONS: [&str; 11] = ["mp4", "m4v", "mkv", "webm", "mov", "avi", "ogv", "mpg", "mpeg", "ts", "wmv"];

pub fn is_video(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Frames shared with the compositor at once: shown, ready next, being read.
pub const SLOTS: usize = 3;

/// What ffprobe says about a video's picture.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    /// As shown (turned, for phone videos).
    pub size: (u32, u32),
    /// Frames per second.
    pub rate: f64,
    /// "h264", "hevc"...
    pub codec: String,
}

pub fn probe(path: &Path) -> Result<Info, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height,avg_frame_rate,r_frame_rate:stream_side_data=rotation", "-of", "default=nw=1"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "videos need FFmpeg (sudo apt install ffmpeg)".to_string() } else { e.to_string() })?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| "no video in it".to_string())
}

fn parse_probe(text: &str) -> Option<Info> {
    let (mut w, mut h, mut rot, mut avg, mut r, mut codec) = (0u32, 0u32, 0i64, 0.0, 0.0, String::new());
    let rate = |v: &str| {
        let (n, d) = v.split_once('/').unwrap_or((v, "1"));
        let (n, d): (f64, f64) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
        (d > 0.0 && n > 0.0).then(|| n / d)
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "codec_name" => codec = v.trim().to_string(),
            "width" => w = v.parse().unwrap_or(0),
            "height" => h = v.parse().unwrap_or(0),
            "rotation" => rot = v.parse::<f64>().unwrap_or(0.0) as i64,
            "avg_frame_rate" => avg = rate(v).unwrap_or(0.0),
            "r_frame_rate" => r = rate(v).unwrap_or(0.0),
            _ => {}
        }
    }
    if w == 0 || h == 0 {
        return None;
    }
    // ffmpeg turns them upright as it decodes.
    let size = if rot.rem_euclid(180) == 90 { (h, w) } else { (w, h) };
    let rate = if avg > 0.0 && avg <= 240.0 { avg } else if r > 0.0 && r <= 240.0 { r } else { 30.0 };
    Some(Info { size, rate, codec })
}

/// A frame from a second in (or the first), at most `max`×`max`, for a
/// thumbnail.
pub fn still(path: &Path, max: u32) -> Result<image::RgbaImage, String> {
    let info = probe(path)?;
    let k = (max as f64 / info.size.0 as f64).min(max as f64 / info.size.1 as f64).min(1.0);
    let (w, h) = (((info.size.0 as f64 * k).round() as u32).max(1), ((info.size.1 as f64 * k).round() as u32).max(1));
    let grab = |from: &str| {
        Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-threads", "2", "-ss", from, "-i"])
            .arg(path)
            .args(["-an", "-frames:v", "1", "-vf", &format!("scale={w}:{h}:flags=area,format=rgba"), "-f", "rawvideo", "pipe:1"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| o.stdout)
            .filter(|px| px.len() == (w * h * 4) as usize)
    };
    // Short videos have no second second.
    let px = grab("1").or_else(|| grab("0")).ok_or("can't read a frame")?;
    image::RgbaImage::from_raw(w, h, px).ok_or_else(|| "bad frame".to_string())
}

/// Whether playing `info` on the screen as `fit` wastes most of the
/// decoding: a video much bigger than the screen shows (a 4K one on a
/// laptop) or faster than screens go.
fn needs_fitting(info: &Info, fit: &Fit) -> bool {
    let (_, _, cw, ch) = fit.crop;
    cw > fit.buf.0 as f64 * 1.25 || ch > fit.buf.1 as f64 * 1.25 || info.rate > 61.0
}

/// Where copies of videos fitted to screens are kept.
fn fitted_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .map(|c| c.join("herowallpaper"))
}

/// The fitted copy's name: the video (path, size, time) and the fit.
fn fitted_name(path: &Path, fit: &Fit) -> String {
    let meta = std::fs::metadata(path).ok();
    let time = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let key = format!("{}\0{}\0{}\0{:?}\0{:?}", path.display(), meta.map_or(0, |m| m.len()), time, fit.crop, fit.buf);
    format!("{:x}.mkv", md5::compute(key.as_bytes()))
}

/// The ffmpeg filter for a fitted copy: the screen's pixels (even, for
/// YUV 4:2:0), at most 60 frames a second.
fn fitted_filter(info: &Info, fit: &Fit) -> String {
    let (x, y, w, h) = fit.crop;
    let even = |v: u32| v.div_ceil(2) * 2;
    let mut f = format!("crop={}:{}:{}:{},scale={}:{}:flags=area", w.round(), h.round(), x.round(), y.round(), even(fit.buf.0), even(fit.buf.1));
    if info.rate > 61.0 {
        f = format!("fps=60,{f}");
    }
    f
}

/// Making a fitted copy, in the background at the lowest priority; it
/// stops (and leaves nothing) when dropped.
pub struct Preparing {
    child: std::sync::Arc<std::sync::Mutex<Option<Child>>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Preparing {
    fn drop(&mut self) {
        self.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

/// Starts making `copy` from `path`; `done` gets `(id, u32::MAX)` when it's
/// there.
fn prepare(path: &Path, copy: &Path, filter: String, (id, done): (u32, std::os::unix::net::UnixStream)) -> std::io::Result<Preparing> {
    let dir = copy.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let part = copy.with_extension("part");
    let (path, copy) = (path.to_path_buf(), copy.to_path_buf());
    let child: std::sync::Arc<std::sync::Mutex<Option<Child>>> = Default::default();
    let cancelled: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
    let (c, stop) = (child.clone(), cancelled.clone());
    std::thread::Builder::new().name("herowallpaper-fit".into()).stack_size(64 * 1024).spawn(move || {
        let _ = std::fs::write(crate::runtime_file("preparing"), path.display().to_string());
        // H.264 made quick to decode; else MPEG-4 (FFmpeg's own).
        let encoders: [&[&str]; 2] = [&["-c:v", "libx264", "-preset", "veryfast", "-tune", "fastdecode", "-crf", "20", "-g", "120"], &["-c:v", "mpeg4", "-q:v", "3"]];
        let mut ok = false;
        for enc in encoders {
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-threads", "2", "-i"]).arg(&path);
            cmd.args(["-an", "-sn", "-dn", "-vf", &filter, "-threads", "2", "-pix_fmt", "yuv420p"]).args(enc).args(["-f", "matroska"]).arg(&part);
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            {
                use std::os::unix::process::CommandExt;
                // The lowest priority: whatever else runs comes first.
                unsafe {
                    cmd.pre_exec(|| {
                        extern "C" {
                            fn setpriority(which: std::ffi::c_int, who: std::ffi::c_uint, prio: std::ffi::c_int) -> std::ffi::c_int;
                        }
                        setpriority(0, 0, 19);
                        Ok(())
                    })
                };
            }
            let Ok(started) = cmd.spawn() else { break };
            *c.lock().unwrap() = Some(started);
            let status = loop {
                if let Some(st) = c.lock().unwrap().as_mut().and_then(|ch| ch.try_wait().ok().flatten()) {
                    break st;
                }
                std::thread::sleep(Duration::from_millis(300));
            };
            *c.lock().unwrap() = None;
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            if status.success() {
                ok = std::fs::rename(&part, &copy).is_ok();
                break;
            }
        }
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(crate::runtime_file("preparing"));
        if ok {
            prune(&dir);
            let mut msg = [0u8; 8];
            msg[..4].copy_from_slice(&id.to_ne_bytes());
            msg[4..].copy_from_slice(&u32::MAX.to_ne_bytes());
            use std::io::Write;
            let _ = (&done).write_all(&msg);
        }
    })?;
    Ok(Preparing { child, cancelled })
}

/// Keeps the fitted copies used last (each use touches its time).
fn prune(dir: &Path) {
    const KEEP: usize = 6;
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut copies: Vec<(std::time::SystemTime, std::path::PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mkv"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    copies.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in copies.into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(p);
    }
}

/// A video playing into `SLOTS` frames.
pub struct Video {
    child: Child,
    /// Asks the reader for the next frame, into a slot.
    ask: Option<mpsc::Sender<usize>>,
    pub frames: Frames,
    pub size: (u32, u32),
    pub dest: (i32, i32, i32, i32),
    pub interval: Duration,
    /// Plays (else its first frame stays).
    pub moving: bool,
    /// A copy fitted to the screen on its way; the first frame shows
    /// meanwhile.
    _preparing: Option<Preparing>,
}

/// The ffmpeg filter for `fit`: crop, scale, frame rate, pixel format.
fn filter(fit: &Fit, rate: Option<f64>) -> String {
    let (x, y, w, h) = fit.crop;
    let mut f = format!("crop={}:{}:{}:{},scale={}:{}:flags=area", w.round(), h.round(), x.round(), y.round(), fit.buf.0, fit.buf.1);
    if let Some(r) = rate {
        // Evenly timed frames, whatever the file does.
        f = format!("fps={r:.3},{f}");
    }
    f + ",format=bgr0"
}

/// The ffmpeg filter for NV12 frames in GPU buffers: the crop (even, as
/// NV12 needs), and only scaled down by ffmpeg when far bigger than the
/// screen shows (the compositor scales the rest as it draws).
fn gpu_filter(fit: &Fit, rate: Option<f64>) -> (String, (u32, u32)) {
    let even = |v: f64| (v.max(0.0) / 2.0).floor() as u32 * 2;
    let (x, y, w, h) = fit.crop;
    let (cw, ch) = (even(w).max(2), even(h).max(2));
    let (cx, cy) = (even(x + (w - cw as f64) / 2.0), even(y + (h - ch as f64) / 2.0));
    let mut f = format!("crop={cw}:{ch}:{cx}:{cy}");
    let mut size = (cw, ch);
    if cw as f64 > fit.buf.0 as f64 * 1.5 && ch as f64 > fit.buf.1 as f64 * 1.5 {
        size = (even(fit.buf.0 as f64).max(2), even(fit.buf.1 as f64).max(2));
        f += &format!(",scale={}:{}:flags=area", size.0, size.1);
    }
    if let Some(r) = rate {
        f = format!("fps={r:.3},{f}");
    }
    (f + ",format=nv12", size)
}

/// How a video gets decoded; tried in turn until one gives a frame.
#[derive(Debug, Clone, PartialEq)]
enum Decoder {
    /// A V4L2 memory-to-memory decoder (most ARM chips' video hardware):
    /// FFmpeg's `<codec>_v4l2m2m`.
    V4l2(String),
    /// What FFmpeg finds (VA-API, VDPAU...).
    Hwaccel,
    Software,
}

/// The decoders to try for `codec`, on this machine.
fn decoders(codec: &str) -> Vec<Decoder> {
    let mut d = vec![];
    if ["h264", "hevc", "vp8", "vp9", "mpeg2video", "mpeg4"].contains(&codec) && v4l2_decoder() {
        let name = if codec == "mpeg2video" { "mpeg2" } else { codec };
        d.push(Decoder::V4l2(format!("{name}_v4l2m2m")));
    }
    d.extend([Decoder::Hwaccel, Decoder::Software]);
    d
}

/// Whether a V4L2 memory-to-memory device is there (webcams aren't).
fn v4l2_decoder() -> bool {
    extern "C" {
        fn ioctl(fd: std::ffi::c_int, req: std::ffi::c_ulong, ...) -> std::ffi::c_int;
    }
    // _IOR('V', 0, struct v4l2_capability): 104 bytes, device_caps at 88.
    const VIDIOC_QUERYCAP: std::ffi::c_ulong = 0x8068_5600;
    const M2M: u32 = 0x4000 | 0x8000;
    let Ok(dir) = std::fs::read_dir("/dev") else { return false };
    dir.flatten().filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("video"))).any(|e| {
        let Ok(f) = std::fs::File::open(e.path()) else { return false };
        let mut cap = [0u8; 104];
        let ok = unsafe { ioctl(f.as_raw_fd(), VIDIOC_QUERYCAP, cap.as_mut_ptr()) } == 0;
        ok && u32::from_ne_bytes(cap[88..92].try_into().unwrap()) & M2M != 0
    })
}

/// Starts ffmpeg writing frames through `filter` to its stdout.
fn ffmpeg(path: &Path, filter: &str, moving: bool, decoder: &Decoder) -> std::io::Result<Child> {
    let mut cmd = Command::new("ffmpeg");
    // Two decoding threads: as fast here, and about 40 MB less than one per
    // core.
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "2", "-filter_threads", "1"]);
    match decoder {
        Decoder::V4l2(name) => {
            cmd.args(["-c:v", name]);
        }
        Decoder::Hwaccel => {
            cmd.args(["-hwaccel", "auto"]);
        }
        Decoder::Software => {}
    }
    if moving {
        cmd.args(["-stream_loop", "-1"]);
    }
    cmd.arg("-i").arg(path).args(["-an", "-sn", "-dn", "-vf", filter]);
    if !moving {
        cmd.args(["-frames:v", "1"]);
    }
    cmd.args(["-f", "rawvideo", "pipe:1"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
}

/// Where the frames are, for the compositor.
pub enum Frames {
    /// One after another, XRGB8888.
    Shm(std::fs::File),
    /// A GPU buffer each, NV12.
    Gpu(Vec<GpuFrame>),
}

pub struct GpuFrame {
    pub fd: std::os::fd::OwnedFd,
    pub stride: u32,
    pub uv_offset: u32,
}

/// Where the reader puts frames.
enum Target {
    Shm(Map, usize),
    Gpu(Vec<crate::gpu::Frame>, Vec<u8>),
}

impl Target {
    fn fill(&mut self, slot: usize, from: &mut impl Read) -> std::io::Result<()> {
        match self {
            Target::Shm(map, frame) => from.read_exact(map.slot(slot, *frame)),
            Target::Gpu(frames, staging) => {
                from.read_exact(staging)?;
                frames[slot].write(staging);
                Ok(())
            }
        }
    }
}

impl Video {
    /// Starts playing; `done` gets `(id, slot)` each time a frame is in.
    /// Returns once the first frame is in slot 0.
    pub fn start(path: &Path, mode: Mode, logical: (i32, i32), scale: f64, mut moving: bool, (id, done): (u32, std::os::unix::net::UnixStream), gpu: Option<&crate::gpu::Gbm>) -> Result<Video, String> {
        let mut info = probe(path)?;
        // Tiling a video makes no sense; it fills the screen instead.
        let mode = if mode == Mode::Tile { Mode::Cover } else { mode };
        let mut fit = picture::fit(mode, info.size, logical, scale);
        // Far more pixels or frames than the screen shows: a copy fitted to
        // it, made once (decoding a 4K video for a laptop screen takes
        // several times the CPU).
        let mut preparing = None;
        let mut path = path.to_path_buf();
        if moving && needs_fitting(&info, &fit) {
            if let Some(copy) = fitted_dir().map(|d| d.join(fitted_name(&path, &fit))) {
                let ready = std::fs::File::options().write(true).open(&copy).and_then(|f| f.set_modified(std::time::SystemTime::now())).is_ok();
                match ready.then(|| probe(&copy)) {
                    Some(Ok(i)) => {
                        fit = picture::fit(mode, i.size, logical, scale);
                        info = i;
                        path = copy;
                    }
                    _ => {
                        let _ = std::fs::remove_file(&copy);
                        if let Ok(p) = prepare(&path, &copy, fitted_filter(&info, &fit), (id, done.try_clone().map_err(|e| e.to_string())?)) {
                            preparing = Some(p);
                            moving = false;
                        }
                    }
                }
            }
        }
        let path = path.as_path();
        let rate = moving.then_some(info.rate);
        // GPU buffers if they can be made at this size, else shared memory.
        let gpu = gpu.and_then(|g| {
            let (filter, size) = gpu_filter(&fit, rate);
            let frames: Vec<_> = (0..SLOTS).map_while(|_| g.nv12(size.0, size.1)).collect();
            (frames.len() == SLOTS).then_some((filter, size, frames))
        });
        let (filter, size, mut target, frames) = match gpu {
            Some((filter, size, frames)) => {
                let shared = frames.iter().map(|f| Ok(GpuFrame { fd: f.fd.try_clone()?, stride: f.stride, uv_offset: f.uv_offset() })).collect::<std::io::Result<Vec<_>>>().map_err(|e| e.to_string())?;
                let bytes = size.0 as usize * (size.1 as usize * 3 / 2);
                (filter, size, Target::Gpu(frames, vec![0; bytes]), Frames::Gpu(shared))
            }
            None => {
                let frame = fit.buf.0 as usize * fit.buf.1 as usize * 4;
                let memory = picture::memfd().map_err(|e| e.to_string())?;
                memory.set_len((frame * SLOTS) as u64).map_err(|e| e.to_string())?;
                let map = Map::new(&memory, frame * SLOTS).map_err(|e| e.to_string())?;
                (filter(&fit, rate), fit.buf, Target::Shm(map, frame), Frames::Shm(memory))
            }
        };
        let frame_bytes = match &target {
            Target::Shm(_, frame) => *frame,
            Target::Gpu(_, staging) => staging.len(),
        };
        // Hardware decoding first; if the first frame doesn't come that way
        // (a decoder that fails on this video or this machine), the next.
        let mut tried = vec![];
        let mut decoders = decoders(&info.codec).into_iter();
        let (child, mut out) = loop {
            let Some(decoder) = decoders.next() else {
                return Err(format!("FFmpeg can't play it ({})", tried.join("; ")));
            };
            let mut child = ffmpeg(path, &filter, moving, &decoder).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "videos need FFmpeg (sudo apt install ffmpeg)".to_string() } else { e.to_string() })?;
            let mut out = child.stdout.take().unwrap();
            // A bigger pipe: fewer, larger reads per frame (up to the 1 MB
            // allowed without privileges).
            extern "C" {
                fn fcntl(fd: std::ffi::c_int, cmd: std::ffi::c_int, ...) -> std::ffi::c_int;
            }
            const F_SETPIPE_SZ: std::ffi::c_int = 1031;
            unsafe { fcntl(out.as_raw_fd(), F_SETPIPE_SZ, frame_bytes.min(1 << 20) as std::ffi::c_int) };
            // The first frame, before anything is shown.
            match target.fill(0, &mut out) {
                Ok(()) => {
                    if std::env::var_os("HEROWALLPAPER_DEBUG").is_some() {
                        eprintln!("herowallpaper: decoding with {decoder:?}: ffmpeg -vf {filter}");
                    }
                    break (child, out);
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    tried.push(e.to_string());
                }
            }
        };
        let (ask, asked) = mpsc::channel::<usize>();
        std::thread::Builder::new()
            .name("herowallpaper-video".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                use std::io::Write;
                let mut done = done;
                for slot in asked {
                    if target.fill(slot, &mut out).is_err() {
                        break;
                    }
                    let mut msg = [0u8; 8];
                    msg[..4].copy_from_slice(&id.to_ne_bytes());
                    msg[4..].copy_from_slice(&(slot as u32).to_ne_bytes());
                    if done.write_all(&msg).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Video { _preparing: preparing, child, ask: Some(ask), frames, size, dest: fit.dest, interval: Duration::from_secs_f64(1.0 / info.rate), moving })
    }

    /// Reads the next frame into `slot` (`done` says when it's in).
    pub fn ask(&self, slot: usize) {
        if let Some(a) = &self.ask {
            let _ = a.send(slot);
        }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        // The reader stops as the pipe closes.
        self.ask = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The frames' memory, mapped here for the reader to write into.
struct Map {
    ptr: *mut u8,
    len: usize,
}

// Only the reader thread touches it once started.
unsafe impl Send for Map {}

extern "C" {
    fn mmap(addr: *mut std::ffi::c_void, len: usize, prot: std::ffi::c_int, flags: std::ffi::c_int, fd: std::ffi::c_int, off: isize) -> *mut std::ffi::c_void;
    fn munmap(addr: *mut std::ffi::c_void, len: usize) -> std::ffi::c_int;
}

impl Map {
    fn new(file: &std::fs::File, len: usize) -> std::io::Result<Map> {
        const PROT_READ: std::ffi::c_int = 1;
        const PROT_WRITE: std::ffi::c_int = 2;
        const MAP_SHARED: std::ffi::c_int = 1;
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, file.as_raw_fd(), 0) };
        if p as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Map { ptr: p.cast(), len })
    }

    #[allow(clippy::mut_from_ref)]
    fn slot(&self, i: usize, frame: usize) -> &mut [u8] {
        assert!((i + 1) * frame <= self.len);
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(i * frame), frame) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output() {
        let i = parse_probe("codec_name=h264\nwidth=1920\nheight=1080\nr_frame_rate=30/1\navg_frame_rate=30000/1001\n").unwrap();
        assert_eq!((i.size, i.codec.as_str()), ((1920, 1080), "h264"));
        assert!((i.rate - 29.97).abs() < 0.01);
        // A phone video, turned.
        let i = parse_probe("width=1920\nheight=1080\navg_frame_rate=0/0\nr_frame_rate=60/1\nrotation=-90\n").unwrap();
        assert_eq!((i.size, i.rate), ((1080, 1920), 60.0));
        assert_eq!(parse_probe("width=0\n"), None);
    }

    #[test]
    fn gpu_filter_crops_evenly() {
        // 4K on a 1280×1024 screen: scaled down by ffmpeg.
        let fit = picture::fit(Mode::Cover, (3840, 2160), (1280, 1024), 1.0);
        assert_eq!(gpu_filter(&fit, Some(30.0)), ("fps=30.000,crop=2700:2160:570:0,scale=1280:1024:flags=area,format=nv12".to_string(), (1280, 1024)));
        // 1080p on 1280×1024: the compositor scales.
        let fit = picture::fit(Mode::Cover, (1920, 1080), (1280, 1024), 1.0);
        assert_eq!(gpu_filter(&fit, None), ("crop=1350:1080:284:0,format=nv12".to_string(), (1350, 1080)));
        // Odd sizes.
        let fit = picture::fit(Mode::Contain, (641, 361), (1920, 1080), 1.0);
        assert_eq!(gpu_filter(&fit, None).1, (640, 360));
    }

    #[test]
    fn filter_crops_then_scales() {
        let fit = picture::fit(Mode::Cover, (3840, 2160), (1280, 1024), 1.0);
        assert_eq!(filter(&fit, Some(30.0)), "fps=30.000,crop=2700:2160:570:0,scale=1280:1024:flags=area,format=bgr0");
    }
}

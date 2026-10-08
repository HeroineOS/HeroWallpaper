//! The Wayland side: a background layer surface per screen holding the
//! background color, with two picture surfaces over it (the one showing,
//! and the next one fading in over it).

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::{self, WlSurface};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1;
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{self, WpFractionalScaleV1};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1};

use crate::config::{Config, Mode, Theme};
use crate::picture;
use crate::gpu::{self, Gbm};
use crate::video::{self, Frames, Video, SLOTS};

#[derive(Default)]
struct Globals {
    compositor: Option<WlCompositor>,
    subcompositor: Option<WlSubcompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    viewporter: Option<WpViewporter>,
    pixel: Option<WpSinglePixelBufferManagerV1>,
    alpha: Option<WpAlphaModifierV1>,
    fractional: Option<WpFractionalScaleManagerV1>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    /// The modifiers NV12 GPU buffers can have.
    nv12: Vec<u64>,
}

/// What a picture was loaded for: the same key, the same pixels (screens
/// alike share them).
#[derive(Debug, Clone, PartialEq)]
struct Key {
    path: PathBuf,
    mode: Mode,
    logical: (i32, i32),
    scale120: u32,
    animate: bool,
    /// A video in GPU buffers.
    gpu: bool,
}

/// A loaded picture: its frames in one shared memory pool.
struct Pic {
    key: Key,
    pool: Option<WlShmPool>,
    buffers: Vec<WlBuffer>,
    delays: Vec<Duration>,
    dest: (i32, i32, i32, i32),
    opaque: bool,
    frame: std::cell::Cell<usize>,
    due: std::cell::Cell<Instant>,
    /// A video: its frames go through `buffers` (one per slot).
    video: Option<Playing>,
    /// To be loaded again (a video's fitted copy is ready).
    stale: std::cell::Cell<bool>,
}

/// A video's frames in flight.
struct Playing {
    video: Video,
    id: u32,
    /// Slots the compositor is done reading.
    released: std::cell::Cell<[bool; SLOTS]>,
    /// The next frame, read and waiting for its time.
    ready: std::cell::Cell<Option<usize>>,
    /// A frame is being read.
    asked: std::cell::Cell<bool>,
}

impl Playing {
    /// Asks for the next frame, into a slot neither shown nor waiting.
    fn ask_next(&self, shown: usize) {
        if self.asked.get() || self.ready.get().is_some() || !self.video.moving {
            return;
        }
        let released = self.released.get();
        if let Some(s) = (0..SLOTS).find(|&s| s != shown && released[s]) {
            self.asked.set(true);
            self.video.ask(s);
        }
    }
}

/// Which video frame a buffer holds.
#[derive(Clone, Copy)]
struct VideoFrame {
    id: u32,
    slot: usize,
}

impl Drop for Pic {
    fn drop(&mut self) {
        for b in &self.buffers {
            b.destroy();
        }
        if let Some(p) = &self.pool {
            p.destroy();
        }
    }
}

/// A surface a picture shows on.
struct Slot {
    surface: WlSurface,
    sub: WlSubsurface,
    viewport: WpViewport,
    alpha: Option<WpAlphaModifierSurfaceV1>,
    pic: Option<Rc<Pic>>,
    /// The compositor hasn't drawn the last change yet (its frame
    /// callback is pending): animations wait for it, so they stop while
    /// the wallpaper is covered.
    waiting: bool,
}

impl Slot {
    fn clear(&mut self) {
        if self.pic.take().is_some() {
            self.surface.attach(None, 0, 0);
            self.surface.commit();
        }
    }

    fn set_alpha(&self, a: f64) {
        if let Some(m) = &self.alpha {
            m.set_multiplier((a.clamp(0.0, 1.0) * u32::MAX as f64) as u32);
        }
    }

    fn show_frame(&mut self, pic: &Pic, qh: &QueueHandle<State>, id: (u32, usize)) {
        self.surface.attach(Some(&pic.buffers[pic.frame.get()]), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.frame(qh, id);
        self.waiting = true;
    }
}

impl Slot {
    fn destroy(&mut self) {
        self.pic = None;
        if let Some(a) = &self.alpha {
            a.destroy();
        }
        self.viewport.destroy();
        self.sub.destroy();
        self.surface.destroy();
    }
}

/// A screen's surfaces.
struct Surf {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    viewport: WpViewport,
    fractional: Option<WpFractionalScaleV1>,
    background: Option<(u32, WlBuffer)>,
    logical: (i32, i32),
    configured: bool,
    /// Preferred scale ×120 (fractional-scale's unit).
    scale120: u32,
    slots: [Slot; 2],
    /// The slot on top: the picture shown, or fading in.
    top: usize,
    /// Fading in since, for how long; and whether the one under fades out.
    fade: Option<(Instant, Duration, bool)>,
}

impl Drop for Surf {
    fn drop(&mut self) {
        if let Some(f) = &self.fractional {
            f.destroy();
        }
        if let Some((_, b)) = &self.background {
            b.destroy();
        }
        // The subsurfaces go before their parent.
        for s in &mut self.slots {
            s.destroy();
        }
        self.viewport.destroy();
        self.layer.destroy();
        self.surface.destroy();
    }
}

struct Screen {
    global: u32,
    output: WlOutput,
    name: Option<String>,
    /// Integer scale (wl_output), when there's no fractional scale.
    scale: i32,
    ready: bool,
    surf: Option<Surf>,
}

pub struct State {
    g: Globals,
    screens: Vec<Screen>,
    config: Config,
    theme: Theme,
    cache: Vec<Weak<Pic>>,
    qh: QueueHandle<State>,
    /// Video readers say here when a frame is in: (id, slot).
    frames_in: std::os::unix::net::UnixStream,
    frames_out: std::os::unix::net::UnixStream,
    next_video: u32,
    /// GPU buffers for videos.
    gpu: Gpu,
}

/// Whether videos go to the GPU (NV12 dmabufs), and with what.
enum Gpu {
    Untried,
    /// The compositor is being asked; the test frame.
    Probing(Rc<Gbm>, u64, #[allow(dead_code)] gpu::Frame),
    /// GBM and the modifier.
    Yes(Rc<Gbm>, u64),
    No,
}

/// The screens' names.
pub fn output_names() -> Result<Vec<String>, String> {
    let (_conn, mut queue, mut state) = connect(Config::default())?;
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    Ok(state.screens.iter().map(|s| s.name.clone().unwrap_or_else(|| "(unnamed)".into())).collect())
}

type Connected = (Connection, wayland_client::EventQueue<State>, State);

fn connect(config: Config) -> Result<Connected, String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland session ({e})"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let (frames_in, frames_out) = std::os::unix::net::UnixStream::pair().map_err(|e| e.to_string())?;
    frames_in.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut state = State { g: Globals::default(), screens: vec![], config, theme: Theme::load(), cache: vec![], qh, frames_in, frames_out, next_video: 0, gpu: Gpu::Untried };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    Ok((conn, queue, state))
}

pub fn run(config_path: PathBuf) -> Result<(), String> {
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        crate::report(Some(&e));
        Config::default()
    });
    one_arena();
    let (conn, mut queue, mut state) = connect(config)?;
    for (have, what) in [
        (state.g.layer_shell.is_some(), "wlr-layer-shell"),
        (state.g.viewporter.is_some(), "wp_viewporter"),
        (state.g.compositor.is_some() && state.g.subcompositor.is_some() && state.g.shm.is_some(), "wl_compositor"),
    ] {
        if !have {
            return Err(format!("the compositor lacks {what}"));
        }
    }
    let mut watch = crate::watch::Watch::new();
    let theme_path = crate::config::theme_path();
    if let Some(w) = &mut watch {
        w.add(&config_path);
        if let Some(t) = &theme_path {
            w.add(t);
        }
    }
    extern "C" {
        fn poll(fds: *mut PollFd, n: std::ffi::c_ulong, timeout: std::ffi::c_int) -> std::ffi::c_int;
    }
    #[repr(C)]
    struct PollFd {
        fd: std::ffi::c_int,
        events: i16,
        revents: i16,
    }
    const POLLIN: i16 = 1;
    loop {
        queue.dispatch_pending(&mut state).map_err(|e| e.to_string())?;
        state.maintain();
        let next = state.tick(Instant::now());
        conn.flush().map_err(|e| e.to_string())?;
        let Some(guard) = queue.prepare_read() else { continue };
        use std::os::fd::AsRawFd;
        let mut fds = [
            PollFd { fd: guard.connection_fd().as_raw_fd(), events: POLLIN, revents: 0 },
            PollFd { fd: watch.as_ref().map_or(-1, |w| w.fd()), events: POLLIN, revents: 0 },
            PollFd { fd: state.frames_in.as_raw_fd(), events: POLLIN, revents: 0 },
        ];
        let timeout = next.map_or(-1, |t| t.saturating_duration_since(Instant::now()).as_micros().div_ceil(1000).min(i32::MAX as u128) as i32);
        let n = unsafe { poll(fds.as_mut_ptr(), 3, timeout) };
        if n < 0 {
            drop(guard);
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.to_string());
        }
        if fds[0].revents != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("the compositor went away ({e})")),
            }
        } else {
            drop(guard);
        }
        if fds[2].revents != 0 {
            state.frames_read();
        }
        if fds[1].revents != 0 {
            if let Some(w) = &watch {
                let changed = w.changed();
                if changed.contains(&0) {
                    match Config::load(&config_path) {
                        Ok(c) => state.config = c,
                        Err(e) => crate::report(Some(&e)),
                    }
                }
                if changed.contains(&1) {
                    state.theme = Theme::load();
                }
                if !changed.is_empty() {
                    state.apply_all();
                }
            }
        }
    }
}

impl State {
    fn animate(&self) -> bool {
        self.config.animate && self.theme.animations
    }

    fn fade_len(&self) -> Duration {
        if self.theme.animations { Duration::from_millis(self.config.transition) } else { Duration::ZERO }
    }

    /// Gives new screens their surfaces.
    fn maintain(&mut self) {
        let (Some(compositor), Some(layer_shell), Some(viewporter), Some(subcompositor)) =
            (&self.g.compositor, &self.g.layer_shell, &self.g.viewporter, &self.g.subcompositor)
        else {
            return;
        };
        let qh = &self.qh;
        for s in &mut self.screens {
            if !s.ready || s.surf.is_some() {
                continue;
            }
            let surface = compositor.create_surface(qh, Some(s.global));
            let layer = layer_shell.get_layer_surface(&surface, Some(&s.output), zwlr_layer_shell_v1::Layer::Background, "herowallpaper".into(), qh, s.global);
            use zwlr_layer_surface_v1::Anchor;
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            // Under the whole screen, panels or not.
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            let viewport = viewporter.get_viewport(&surface, qh, ());
            let fractional = self.g.fractional.as_ref().map(|f| f.get_fractional_scale(&surface, qh, s.global));
            let slot = |_| {
                let child = compositor.create_surface(qh, None);
                let sub = subcompositor.get_subsurface(&child, &surface, qh, ());
                // Each updates on its own (animation frames, fades).
                sub.set_desync();
                // Clicks go to the desktop below, as on the background.
                let empty = compositor.create_region(qh, ());
                child.set_input_region(Some(&empty));
                empty.destroy();
                Slot {
                    viewport: viewporter.get_viewport(&child, qh, ()),
                    alpha: self.g.alpha.as_ref().map(|a| a.get_surface(&child, qh, ())),
                    surface: child,
                    sub,
                    pic: None,
                    waiting: false,
                }
            };
            surface.commit();
            s.surf = Some(Surf {
                slots: [0, 1].map(slot),
                surface,
                layer,
                viewport,
                fractional,
                background: None,
                logical: (0, 0),
                configured: false,
                scale120: 120 * s.scale.max(1) as u32,
                top: 0,
                fade: None,
            });
        }
    }

    fn apply_all(&mut self) {
        for i in 0..self.screens.len() {
            self.apply(i);
        }
    }

    /// Shows what the config says on screen `i`.
    fn apply(&mut self, i: usize) {
        let animate = self.animate();
        let fade_len = self.fade_len();
        let budget = self.config.animation_memory.saturating_mul(1 << 20).min(isize::MAX as u64 / 2) as usize;
        let s = &self.screens[i];
        let Some(surf) = &s.surf else { return };
        if !surf.configured {
            return;
        }
        let shown = self.config.for_output(s.name.as_deref().unwrap_or(""));
        self.set_background(i, shown.color.unwrap_or(self.theme.background));
        let video = shown.path.as_deref().is_some_and(video::is_video);
        if video && !self.gpu_known() {
            // Shown once the compositor says about GPU buffers.
            return;
        }
        let gpu = video && matches!(self.gpu, Gpu::Yes(..));
        let surf = self.screens[i].surf.as_ref().unwrap();
        let want = shown.path.map(|path| Key { path, mode: shown.mode, logical: surf.logical, scale120: surf.scale120, animate, gpu });
        let have = surf.slots[surf.top].pic.as_ref().filter(|p| !p.stale.get()).map(|p| &p.key);
        if want.as_ref() == have {
            return;
        }
        let Some(key) = want else {
            let surf = self.screens[i].surf.as_mut().unwrap();
            surf.fade = None;
            surf.slots.iter_mut().for_each(Slot::clear);
            return;
        };
        let pic = match self.picture(&key, budget) {
            Ok(p) => {
                crate::report(None);
                p
            }
            Err(e) => {
                crate::report(Some(&e));
                return;
            }
        };
        let global = self.screens[i].global;
        let qh = self.qh.clone();
        let surf = self.screens[i].surf.as_mut().unwrap();
        let fading = !fade_len.is_zero() && surf.slots.iter().all(|s| s.alpha.is_some());
        // Covers the screen (and hides what's under once in).
        let covers = pic.dest == (0, 0, surf.logical.0, surf.logical.1) && pic.opaque;
        let new = 1 - surf.top;
        // A fade still going: the one fading in stays, the older one goes.
        if surf.fade.take().is_some() {
            surf.slots[new].clear();
        }
        let (a, b) = surf.slots.split_at_mut(1);
        let (slot, under) = if new == 0 { (&mut a[0], &b[0]) } else { (&mut b[0], &a[0]) };
        slot.pic = Some(pic.clone());
        slot.viewport.set_destination(pic.dest.2, pic.dest.3);
        slot.sub.set_position(pic.dest.0, pic.dest.1);
        slot.sub.place_above(&under.surface);
        slot.set_alpha(if fading { 0.0 } else { 1.0 });
        slot.show_frame(&pic, &qh, (global, new));
        slot.surface.commit();
        surf.top = new;
        // Applies the position and order.
        surf.surface.commit();
        if fading {
            surf.fade = Some((Instant::now(), fade_len, !covers));
        } else {
            surf.slots[1 - new].clear();
        }
    }

    /// Loaded already for another screen, or loads it.
    fn picture(&mut self, key: &Key, budget: usize) -> Result<Rc<Pic>, String> {
        self.cache.retain(|w| w.upgrade().is_some_and(|p| !p.stale.get()));
        if let Some(p) = self.cache.iter().filter_map(Weak::upgrade).find(|p| &p.key == key) {
            return Ok(p);
        }
        if video::is_video(&key.path) {
            return self.video(key);
        }
        let loaded = picture::load(&picture::Request {
            path: &key.path,
            mode: key.mode,
            logical: key.logical,
            scale: key.scale120 as f64 / 120.0,
            animate: key.animate,
            budget,
        })?;
        let shm = self.g.shm.as_ref().unwrap();
        let bytes = loaded.frame_bytes();
        let total = i32::try_from(bytes * loaded.delays.len()).map_err(|_| "too large".to_string())?;
        let pool = shm.create_pool(loaded.memory.as_fd(), total, &self.qh, ());
        let format = if loaded.opaque { wl_shm::Format::Xrgb8888 } else { wl_shm::Format::Argb8888 };
        let (w, h) = (loaded.size.0 as i32, loaded.size.1 as i32);
        let buffers = (0..loaded.delays.len()).map(|f| pool.create_buffer((f * bytes) as i32, w, h, w * 4, format, &self.qh, ())).collect();
        let pic = Rc::new(Pic {
            key: key.clone(),
            pool: Some(pool),
            buffers,
            due: std::cell::Cell::new(Instant::now() + loaded.delays[0]),
            delays: loaded.delays,
            dest: loaded.dest,
            opaque: loaded.opaque,
            frame: std::cell::Cell::new(0),
            video: None,
            stale: Default::default(),
        });
        self.cache.push(Rc::downgrade(&pic));
        trim();
        Ok(pic)
    }

    /// Starts a video (its first frame in, for showing right away).
    fn video(&mut self, key: &Key) -> Result<Rc<Pic>, String> {
        let id = self.next_video;
        self.next_video += 1;
        let out = self.frames_out.try_clone().map_err(|e| e.to_string())?;
        let gpu = match &self.gpu {
            Gpu::Yes(g, m) if key.gpu => Some((g.clone(), *m)),
            _ => None,
        };
        let v = Video::start(&key.path, key.mode, key.logical, key.scale120 as f64 / 120.0, key.animate, (id, out), gpu.as_ref().map(|(g, _)| &**g)).map_err(|e| format!("{}: {e}", key.path.display()))?;
        let (w, h) = (v.size.0 as i32, v.size.1 as i32);
        let (pool, buffers) = match &v.frames {
            Frames::Shm(memory) => {
                let bytes = v.size.0 as usize * v.size.1 as usize * 4;
                let total = i32::try_from(bytes * SLOTS).map_err(|_| "too large".to_string())?;
                let pool = self.g.shm.as_ref().unwrap().create_pool(memory.as_fd(), total, &self.qh, ());
                let buffers = (0..SLOTS).map(|slot| pool.create_buffer((slot * bytes) as i32, w, h, w * 4, wl_shm::Format::Xrgb8888, &self.qh, VideoFrame { id, slot })).collect();
                (Some(pool), buffers)
            }
            Frames::Gpu(frames) => {
                let modifier = gpu.map_or(gpu::MOD_LINEAR, |(_, m)| m);
                let dmabuf = self.g.dmabuf.as_ref().unwrap();
                let buffers = frames
                    .iter()
                    .enumerate()
                    .map(|(slot, f)| {
                        let params = dmabuf.create_params(&self.qh, false);
                        let (hi, lo) = ((modifier >> 32) as u32, modifier as u32);
                        params.add(f.fd.as_fd(), 0, 0, f.stride, hi, lo);
                        params.add(f.fd.as_fd(), 1, f.uv_offset, f.stride, hi, lo);
                        let b = params.create_immed(w, h, gpu::NV12, zwp_linux_buffer_params_v1::Flags::empty(), &self.qh, VideoFrame { id, slot });
                        params.destroy();
                        b
                    })
                    .collect();
                (None, buffers)
            }
        };
        let playing = Playing { id, released: std::cell::Cell::new([false, true, true]), ready: std::cell::Cell::new(None), asked: std::cell::Cell::new(false), video: v };
        // The second frame on its way already.
        playing.ask_next(0);
        let pic = Rc::new(Pic {
            key: key.clone(),
            pool,
            buffers,
            delays: vec![playing.video.interval],
            dest: playing.video.dest,
            opaque: true,
            frame: std::cell::Cell::new(0),
            due: std::cell::Cell::new(Instant::now() + playing.video.interval),
            video: Some(playing),
            stale: Default::default(),
        });
        self.cache.push(Rc::downgrade(&pic));
        Ok(pic)
    }

    /// Whether a video can start, starting the GPU buffer test first: a
    /// small buffer the compositor answers about (a failed real one would
    /// end the connection). The video starts when it has.
    fn gpu_known(&mut self) -> bool {
        if let Gpu::Untried = self.gpu {
            self.gpu = self.probe().unwrap_or(Gpu::No);
        }
        !matches!(self.gpu, Gpu::Probing(..))
    }

    fn probe(&self) -> Option<Gpu> {
        let no = |why: &str| {
            crate::videos_play(&format!("through shared memory ({why})"));
            None
        };
        if std::env::var_os("HEROWALLPAPER_NO_GPU").is_some() {
            return no("HEROWALLPAPER_NO_GPU is set");
        }
        let Some(dmabuf) = self.g.dmabuf.as_ref() else { return no("the compositor takes no GPU buffers") };
        let Some(modifier) = [gpu::MOD_LINEAR, gpu::MOD_INVALID].into_iter().find(|m| self.g.nv12.contains(m)) else { return no("the compositor takes no NV12 GPU buffers") };
        let Some(gbm) = Gbm::open(None) else { return no("no GBM: libgbm1 or a GPU driver for it is missing") };
        let frame = gbm.nv12(64, 64)?;
        let params = dmabuf.create_params(&self.qh, true);
        let (hi, lo) = ((modifier >> 32) as u32, modifier as u32);
        params.add(frame.fd.as_fd(), 0, 0, frame.stride, hi, lo);
        params.add(frame.fd.as_fd(), 1, frame.uv_offset(), frame.stride, hi, lo);
        params.create(64, 64, gpu::NV12, zwp_linux_buffer_params_v1::Flags::empty());
        Some(Gpu::Probing(Rc::new(gbm), modifier, frame))
    }

    /// Every picture showing somewhere (each once).
    fn shown_pics(&self) -> Vec<Rc<Pic>> {
        let mut pics: Vec<Rc<Pic>> = vec![];
        for s in &self.screens {
            for slot in s.surf.iter().flat_map(|f| f.slots.iter()) {
                if let Some(p) = &slot.pic {
                    if !pics.iter().any(|q| Rc::ptr_eq(p, q)) {
                        pics.push(p.clone());
                    }
                }
            }
        }
        pics
    }

    /// Frames the video readers finished.
    fn frames_read(&mut self) {
        let mut buf = [0u8; 8 * 16];
        loop {
            let n = match std::io::Read::read(&mut &self.frames_in, &mut buf) {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            for msg in buf[..n - n % 8].chunks_exact(8) {
                let id = u32::from_ne_bytes(msg[..4].try_into().unwrap());
                let slot = u32::from_ne_bytes(msg[4..].try_into().unwrap()) as usize;
                if slot == u32::MAX as usize {
                    // A fitted copy is ready: it replaces what showed meanwhile.
                    for pic in self.shown_pics() {
                        if pic.video.as_ref().is_some_and(|p| p.id == id) {
                            pic.stale.set(true);
                        }
                    }
                    self.apply_all();
                    continue;
                }
                for pic in self.shown_pics() {
                    if let Some(p) = pic.video.as_ref().filter(|p| p.id == id) {
                        p.asked.set(false);
                        p.ready.set(Some(slot));
                    }
                }
            }
        }
    }

    /// The color under the picture: one pixel, stretched over the screen.
    fn set_background(&mut self, i: usize, color: u32) {
        let qh = self.qh.clone();
        let surf = self.screens[i].surf.as_mut().unwrap();
        if surf.background.as_ref().is_some_and(|(c, _)| *c == color) {
            return;
        }
        let ch = |shift: u32| ((color >> shift) & 0xff) * 0x0101_0101;
        let buffer = match &self.g.pixel {
            Some(p) => p.create_u32_rgba_buffer(ch(16), ch(8), ch(0), u32::MAX, &qh, ()),
            None => {
                // A 1×1 shm buffer, where single-pixel buffers aren't.
                use std::io::Write;
                let mut f = picture::memfd().expect("memfd");
                let _ = f.write_all(&(color | 0xff00_0000).to_le_bytes());
                let pool = self.g.shm.as_ref().unwrap().create_pool(f.as_fd(), 4, &qh, ());
                let b = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Xrgb8888, &qh, ());
                pool.destroy();
                b
            }
        };
        surf.surface.attach(Some(&buffer), 0, 0);
        surf.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        surf.surface.commit();
        if let Some((_, old)) = surf.background.replace((color, buffer)) {
            old.destroy();
        }
    }

    /// Moves fades and animations on; returns when it next has to.
    fn tick(&mut self, now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut soonest = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
        let qh = self.qh.clone();
        // Fades step as the compositor draws them (frame callbacks); one
        // it doesn't draw (screen off, covered) ends on time anyway.
        for s in &mut self.screens {
            let Some(surf) = &mut s.surf else { continue };
            let Some((start, len, under)) = surf.fade else { continue };
            let top = surf.top;
            let t = now.duration_since(start).as_secs_f64() / len.as_secs_f64();
            if t >= 1.0 {
                surf.fade = None;
                surf.slots[top].set_alpha(1.0);
                surf.slots[top].surface.commit();
                surf.slots[1 - top].clear();
                continue;
            }
            if !surf.slots[top].waiting {
                let a = t * t * (3.0 - 2.0 * t);
                surf.slots[top].set_alpha(a);
                if under {
                    surf.slots[1 - top].set_alpha(1.0 - a);
                    surf.slots[1 - top].surface.commit();
                }
                let slot = &mut surf.slots[top];
                slot.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
                slot.surface.frame(&qh, (s.global, top));
                slot.waiting = true;
                slot.surface.commit();
            }
            soonest(start + len);
        }
        if !self.animate() {
            return next;
        }
        // Animated pictures and videos (each once, however many screens
        // show it).
        let pics: Vec<Rc<Pic>> = self.shown_pics().into_iter().filter(|p| p.buffers.len() > 1).collect();
        for pic in pics {
            let shows = |slot: &Slot| slot.pic.as_ref().is_some_and(|p| Rc::ptr_eq(p, &pic));
            // Only while the compositor is drawing it.
            let drawn = self.screens.iter().flat_map(|s| s.surf.iter().flat_map(|f| f.slots.iter())).any(|slot| shows(slot) && !slot.waiting);
            if !drawn {
                continue;
            }
            if let Some(p) = &pic.video {
                // Read ahead, but only while drawn: covered, ffmpeg waits.
                p.ask_next(pic.frame.get());
                if p.ready.get().is_none() {
                    // Woken by the reader when it's in.
                    continue;
                }
            }
            if now < pic.due.get() {
                soonest(pic.due.get());
                continue;
            }
            let f = match &pic.video {
                Some(p) => {
                    let f = p.ready.take().unwrap();
                    let mut r = p.released.get();
                    r[f] = false;
                    p.released.set(r);
                    f
                }
                None => (pic.frame.get() + 1) % pic.buffers.len(),
            };
            pic.frame.set(f);
            let delay = pic.delays[f.min(pic.delays.len() - 1)];
            let due = pic.due.get() + delay;
            // Late (it was covered): from now on.
            pic.due.set(if due < now { now + delay } else { due });
            for s in &mut self.screens {
                let global = s.global;
                let Some(surf) = &mut s.surf else { continue };
                for (i, slot) in surf.slots.iter_mut().enumerate() {
                    if shows(slot) {
                        slot.show_frame(&pic, &qh, (global, i));
                        slot.surface.commit();
                    }
                }
            }
            if let Some(p) = &pic.video {
                p.ask_next(f);
            }
            soonest(pic.due.get());
        }
        next
    }
}

/// Hands memory freed after decoding back to the system (glibc keeps it
/// for reuse otherwise; there's no more decoding until the next change).
fn trim() {
    #[cfg(target_env = "gnu")]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        unsafe { malloc_trim(0) };
    }
}

/// One malloc arena for every thread, so decoders' worker threads (AVIF)
/// leave nothing behind in arenas of their own.
pub fn one_arena() {
    #[cfg(target_env = "gnu")]
    {
        extern "C" {
            fn mallopt(param: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
        }
        const M_ARENA_MAX: std::ffi::c_int = -8;
        unsafe { mallopt(M_ARENA_MAX, 1) };
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                let g = &mut state.g;
                match interface.as_str() {
                    "wl_compositor" => g.compositor = Some(registry.bind(name, version.min(6), qh, ())),
                    "wl_subcompositor" => g.subcompositor = Some(registry.bind(name, 1, qh, ())),
                    "wl_shm" => g.shm = Some(registry.bind(name, 1, qh, ())),
                    "zwlr_layer_shell_v1" => g.layer_shell = Some(registry.bind(name, version.min(4), qh, ())),
                    "wp_viewporter" => g.viewporter = Some(registry.bind(name, 1, qh, ())),
                    "wp_single_pixel_buffer_manager_v1" => g.pixel = Some(registry.bind(name, 1, qh, ())),
                    "wp_alpha_modifier_v1" => g.alpha = Some(registry.bind(name, 1, qh, ())),
                    "wp_fractional_scale_manager_v1" => g.fractional = Some(registry.bind(name, 1, qh, ())),
                    // Version 3 lists formats and modifiers right away.
                    "zwp_linux_dmabuf_v1" if version >= 3 => g.dmabuf = Some(registry.bind(name, 3, qh, ())),
                    "wl_output" => {
                        let output = registry.bind(name, version.min(4), qh, name);
                        state.screens.push(Screen { global: name, output, name: None, scale: 1, ready: false, surf: None });
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.screens.iter().position(|s| s.global == name) {
                    let s = state.screens.remove(i);
                    if s.output.version() >= 3 {
                        s.output.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(s) = state.screens.iter_mut().find(|s| s.global == *global) else { return };
        match event {
            wl_output::Event::Name { name } => s.name = Some(name),
            wl_output::Event::Scale { factor } => s.scale = factor,
            wl_output::Event::Done => {
                let renamed = s.ready;
                s.ready = true;
                if renamed {
                    if let Some(i) = state.screens.iter().position(|s| s.global == *global) {
                        state.apply(i);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for State {
    fn event(state: &mut Self, layer: &ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, global: &u32, _: &Connection, qh: &QueueHandle<Self>) {
        let Some(i) = state.screens.iter().position(|s| s.global == *global) else { return };
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                let surf = state.screens[i].surf.as_mut().unwrap();
                let size = (width as i32, height as i32);
                let first = !surf.configured;
                surf.configured = true;
                if size != surf.logical {
                    surf.logical = size;
                    surf.viewport.set_destination(size.0.max(1), size.1.max(1));
                    let region: WlRegion = state.g.compositor.as_ref().unwrap().create_region(qh, ());
                    region.add(0, 0, size.0, size.1);
                    surf.surface.set_opaque_region(Some(&region));
                    region.destroy();
                } else if !first {
                    surf.surface.commit();
                    return;
                }
                if first {
                    // The first commit after configuring needs a buffer.
                    surf.background = None;
                }
                state.apply(i);
                if let Some(surf) = &state.screens[i].surf {
                    surf.surface.commit();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.screens[i].surf = None,
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, u32> for State {
    fn event(state: &mut Self, _: &WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else { return };
        let Some(i) = state.screens.iter().position(|s| s.global == *global) else { return };
        let surf = state.screens[i].surf.as_mut().unwrap();
        if surf.scale120 != scale {
            surf.scale120 = scale;
            state.apply(i);
        }
    }
}

impl Dispatch<WlSurface, Option<u32>> for State {
    fn event(state: &mut Self, _: &WlSurface, event: wl_surface::Event, global: &Option<u32>, _: &Connection, _: &QueueHandle<Self>) {
        // Without fractional scaling: the integer scale it prefers.
        let (wl_surface::Event::PreferredBufferScale { factor }, Some(global)) = (event, global) else { return };
        if state.g.fractional.is_some() {
            return;
        }
        let Some(i) = state.screens.iter().position(|s| s.global == *global) else { return };
        let surf = state.screens[i].surf.as_mut().unwrap();
        if surf.scale120 != 120 * factor.max(1) as u32 {
            surf.scale120 = 120 * factor.max(1) as u32;
            state.apply(i);
        }
    }
}

impl Dispatch<WlBuffer, VideoFrame> for State {
    fn event(state: &mut Self, _: &WlBuffer, event: wayland_client::protocol::wl_buffer::Event, f: &VideoFrame, _: &Connection, _: &QueueHandle<Self>) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            for pic in state.shown_pics() {
                if let Some(p) = pic.video.as_ref().filter(|p| p.id == f.id) {
                    let mut r = p.released.get();
                    r[f.slot] = true;
                    p.released.set(r);
                }
            }
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(state: &mut Self, _: &ZwpLinuxDmabufV1, event: zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } if format == gpu::NV12 => state.g.nv12.push((modifier_hi as u64) << 32 | modifier_lo as u64),
            // Version 1 and 2: implicit modifiers only.
            zwp_linux_dmabuf_v1::Event::Format { format } if format == gpu::NV12 => state.g.nv12.push(gpu::MOD_INVALID),
            _ => {}
        }
    }
}

/// `true` for the test buffer.
impl Dispatch<ZwpLinuxBufferParamsV1, bool> for State {
    fn event(state: &mut Self, params: &ZwpLinuxBufferParamsV1, event: zwp_linux_buffer_params_v1::Event, test: &bool, _: &Connection, _: &QueueHandle<Self>) {
        let ok = match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                buffer.destroy();
                true
            }
            zwp_linux_buffer_params_v1::Event::Failed => false,
            _ => return,
        };
        if *test {
            params.destroy();
        }
        let was = std::mem::replace(&mut state.gpu, Gpu::No);
        if ok {
            if let Gpu::Probing(gbm, modifier, _) = was {
                state.gpu = Gpu::Yes(gbm, modifier);
                crate::videos_play("in GPU memory (NV12)");
            }
        } else {
            crate::videos_play("through shared memory (the compositor turned down NV12 GPU buffers)");
        }
        // Videos waiting start; ones in GPU buffers that failed restart.
        state.apply_all();
    }

    wayland_client::event_created_child!(State, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ()),
    ]);
}

impl Dispatch<WlCallback, (u32, usize)> for State {
    fn event(state: &mut Self, _: &WlCallback, event: wl_callback::Event, &(global, slot): &(u32, usize), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            if let Some(surf) = state.screens.iter_mut().find(|s| s.global == global).and_then(|s| s.surf.as_mut()) {
                surf.slots[slot].waiting = false;
            }
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSubcompositor);
delegate_noop!(State: ignore WlSubsurface);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlRegion);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WpSinglePixelBufferManagerV1);
delegate_noop!(State: ignore WpAlphaModifierV1);
delegate_noop!(State: ignore WpAlphaModifierSurfaceV1);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);

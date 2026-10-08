//! Video frames in GPU memory (dmabufs), as NV12: the YUV video decoders
//! put out, so ffmpeg skips converting each frame to RGB, the compositor
//! converts and scales it on the GPU as it draws, and doesn't copy each
//! frame up to the GPU either. The buffers come from GBM (Mesa's, loaded
//! when a video first plays; no GBM, no GPU or a compositor that can't
//! take them: frames go through shared memory as RGB instead).

use std::ffi::{c_char, c_int, c_void, CStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// DRM fourcc of NV12 (a Y plane, then a half-size plane of U and V pairs).
pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const MOD_LINEAR: u64 = 0;
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const GBM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

extern "C" {
    fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: isize) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn ioctl(fd: c_int, req: std::ffi::c_ulong, ...) -> c_int;
}

/// A GBM device on the GPU's render node.
pub struct Gbm {
    device: *mut c_void,
    _node: std::fs::File,
    bo_create: unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void,
    bo_get_fd: unsafe extern "C" fn(*mut c_void) -> c_int,
    bo_get_stride: unsafe extern "C" fn(*mut c_void) -> u32,
    bo_destroy: unsafe extern "C" fn(*mut c_void),
    device_destroy: unsafe extern "C" fn(*mut c_void),
}

impl Gbm {
    /// The first render node GBM opens (`dev`: the one the compositor named).
    pub fn open(dev: Option<u64>) -> Option<Gbm> {
        const RTLD_NOW: c_int = 2;
        let lib = unsafe { dlopen(c"libgbm.so.1".as_ptr(), RTLD_NOW) };
        if lib.is_null() {
            return None;
        }
        let sym = |name: &CStr| {
            let p = unsafe { dlsym(lib, name.as_ptr()) };
            (!p.is_null()).then_some(p)
        };
        let create_device: unsafe extern "C" fn(c_int) -> *mut c_void = unsafe { std::mem::transmute(sym(c"gbm_create_device")?) };
        let (bo_create, bo_get_fd, bo_get_stride, bo_destroy, device_destroy) = unsafe {
            (
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void>(sym(c"gbm_bo_create")?),
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void) -> c_int>(sym(c"gbm_bo_get_fd")?),
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void) -> u32>(sym(c"gbm_bo_get_stride")?),
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void)>(sym(c"gbm_bo_destroy")?),
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void)>(sym(c"gbm_device_destroy")?),
            )
        };
        let mut nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri").ok()?.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("renderD"))).collect();
        nodes.sort();
        // The compositor's GPU first.
        if let Some(dev) = dev {
            use std::os::unix::fs::MetadataExt;
            nodes.sort_by_key(|p| std::fs::metadata(p).map_or(true, |m| m.rdev() != dev));
        }
        for path in nodes {
            let Ok(node) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { continue };
            let device = unsafe { create_device(node.as_raw_fd()) };
            if device.is_null() {
                continue;
            }
            let gbm = Gbm { device, _node: node, bo_create, bo_get_fd, bo_get_stride, bo_destroy, device_destroy };
            // One that can make the buffers (drivers without GBM say yes to
            // the device and no to buffers).
            if gbm.nv12(64, 64).is_some() {
                return Some(gbm);
            }
        }
        None
    }

    /// A `w`×`h` NV12 frame (even sizes) in one linear buffer: the Y rows,
    /// then the UV rows, `stride` bytes apart.
    pub fn nv12(&self, w: u32, h: u32) -> Option<Frame> {
        // As one 8-bit plane 1.5 times as tall: every driver makes those
        // linear, where NV12 itself isn't always offered.
        let rows = h + h / 2;
        let bo = [GBM_BO_USE_LINEAR, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING].iter().map(|&flags| unsafe { (self.bo_create)(self.device, w, rows, GBM_FORMAT_R8, flags) }).find(|b| !b.is_null())?;
        let (fd, stride) = unsafe { ((self.bo_get_fd)(bo), (self.bo_get_stride)(bo)) };
        // The dmabuf keeps the memory; GBM's handle on it can go.
        unsafe { (self.bo_destroy)(bo) };
        if fd < 0 || stride < w {
            return None;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let len = stride as usize * rows as usize;
        const PROT_WRITE: c_int = 2;
        const MAP_SHARED: c_int = 1;
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_WRITE, MAP_SHARED, fd.as_raw_fd(), 0) };
        if ptr as isize == -1 {
            return None;
        }
        Some(Frame { fd, ptr: ptr.cast(), len, stride, size: (w, h) })
    }
}

impl Drop for Gbm {
    fn drop(&mut self) {
        unsafe { (self.device_destroy)(self.device) };
    }
}

/// One frame's dmabuf, mapped for writing.
pub struct Frame {
    pub fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
    pub stride: u32,
    pub size: (u32, u32),
}

// Written only by the reader thread once playing.
unsafe impl Send for Frame {}

impl Frame {
    /// Where the UV rows start.
    pub fn uv_offset(&self) -> u32 {
        self.stride * self.size.1
    }

    /// Copies a tightly packed NV12 frame in.
    pub fn write(&mut self, px: &[u8]) {
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        let rows = h + h / 2;
        assert!(px.len() >= w * rows);
        let dst = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) };
        self.sync(false);
        let stride = self.stride as usize;
        if stride == w {
            dst[..w * rows].copy_from_slice(&px[..w * rows]);
        } else {
            for (d, s) in dst.chunks_exact_mut(stride).zip(px.chunks_exact(w)).take(rows) {
                d[..w].copy_from_slice(s);
            }
        }
        self.sync(true);
    }

    /// Tells the driver the CPU writes (caches flushed after).
    fn sync(&self, end: bool) {
        #[repr(C)]
        struct DmaBufSync {
            flags: u64,
        }
        const WRITE: u64 = 2;
        const END: u64 = 4;
        // _IOW('b', 0, struct dma_buf_sync)
        const DMA_BUF_IOCTL_SYNC: std::ffi::c_ulong = 0x4008_6200;
        let s = DmaBufSync { flags: WRITE | if end { END } else { 0 } };
        unsafe { ioctl(self.fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &s as *const DmaBufSync) };
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_land_at_the_stride() {
        // Shared memory standing in for a dmabuf (the sync ioctl just fails).
        let (w, h, stride) = (6u32, 4u32, 8u32);
        let rows = (h + h / 2) as usize;
        let file = crate::picture::memfd().unwrap();
        file.set_len((stride as usize * rows) as u64).unwrap();
        let ptr = unsafe { mmap(std::ptr::null_mut(), stride as usize * rows, 3, 1, file.as_raw_fd(), 0) };
        let mut f = Frame { fd: file.into(), ptr: ptr.cast(), len: stride as usize * rows, stride, size: (w, h) };
        let px: Vec<u8> = (0..w as usize * rows).map(|i| i as u8 + 1).collect();
        f.write(&px);
        let got = unsafe { std::slice::from_raw_parts(f.ptr, f.len) };
        for r in 0..rows {
            assert_eq!(&got[r * 8..r * 8 + 6], &px[r * 6..r * 6 + 6]);
            assert_eq!(&got[r * 8 + 6..r * 8 + 8], &[0, 0]);
        }
        assert_eq!(f.uv_offset(), 32);
    }

    #[test]
    fn fourcc() {
        assert_eq!(super::NV12, 0x3231_564e);
        assert_eq!(super::GBM_FORMAT_R8, 0x2020_3852);
    }
}

//! Noticing when the config or theme file is saved (inotify on their
//! directories: editors and `set` replace files rather than write them).

use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

extern "C" {
    fn inotify_init1(flags: c_int) -> c_int;
    fn inotify_add_watch(fd: c_int, path: *const c_char, mask: u32) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
}

const IN_NONBLOCK: c_int = 0o4000;
const IN_CLOEXEC: c_int = 0o2000000;
const IN_CLOSE_WRITE: u32 = 0x8;
const IN_MOVED_TO: u32 = 0x80;
const IN_CREATE: u32 = 0x100;

pub struct Watch {
    fd: OwnedFd,
    /// (watch descriptor, file name)
    files: Vec<(c_int, Vec<u8>)>,
}

impl Watch {
    pub fn new() -> Option<Watch> {
        let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        (fd >= 0).then(|| Watch { fd: unsafe { OwnedFd::from_raw_fd(fd) }, files: vec![] })
    }

    /// Watches for `file` being written; returns false if it can't.
    pub fn add(&mut self, file: &Path) -> bool {
        let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else { return false };
        // The directory has to exist to be watched.
        let _ = std::fs::create_dir_all(dir);
        let Ok(c) = CString::new(dir.as_os_str().as_encoded_bytes()) else { return false };
        let wd = unsafe { inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE) };
        if wd < 0 {
            return false;
        }
        self.files.push((wd, name.as_encoded_bytes().to_vec()));
        true
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Which watched files changed since last asked (indexes, in the
    /// order added).
    pub fn changed(&self) -> Vec<usize> {
        let mut out = vec![];
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut at = 0;
            while at + 16 <= n as usize {
                let wd = c_int::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                let name = &buf[at + 16..(at + 16 + len).min(n as usize)];
                let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                for (i, (w, f)) in self.files.iter().enumerate() {
                    if *w == wd && f == name && !out.contains(&i) {
                        out.push(i);
                    }
                }
                at += 16 + len;
            }
        }
        out
    }
}

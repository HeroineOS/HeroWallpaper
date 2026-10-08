//! HeroWallpaper: a wallpaper for Wayland compositors with wlr-layer-shell
//! (HeroWM, sway, Hyprland, KDE...). Still and animated pictures, a
//! crossfade between them, a picture per screen.
//!
//! Light by leaving the work to the compositor: each screen is a
//! one-pixel buffer of the background color (stretched over the screen)
//! under the picture, the picture is scaled up by the compositor
//! (wp_viewporter) and faded by it (wp_alpha_modifier), so between
//! changes nothing runs here: the process sleeps in poll() until the
//! config or theme file changes, or an animated picture's next frame
//! is due (and only while the compositor shows it).

mod config;
mod gpu;
mod picture;
mod thumb;
mod video;
mod watch;
mod wl;

use std::path::PathBuf;

fn usage() -> ! {
    println!(
        "herowallpaper {} - the HeroineOS wallpaper (pictures, animations, videos)

Usage:
  herowallpaper                  run (one per session)
  herowallpaper set PICTURE      show PICTURE (an image file; \"\" for none)
      [--mode cover|contain|stretch|center|tile] [--output NAME]
  herowallpaper --outputs        list the screens' names
  herowallpaper --running        exit status 0 if running in this session
  herowallpaper --status         whether it runs, which version, and what went wrong last
  herowallpaper thumbnail FILE...  print a thumbnail of each (made if needed)
  herowallpaper --print-default-config
  herowallpaper --version
  herowallpaper --help

Settings: ~/.config/hero/wallpaper.toml (applied when saved).

Environment:
  HEROWALLPAPER_NO_GPU=1         videos through shared memory, not GPU buffers
  HEROWALLPAPER_DEBUG=1          print how videos are decoded and shown",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(0)
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("herowallpaper: {msg}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {}
        Some("-h" | "--help") => usage(),
        Some("-V" | "--version") => {
            println!("herowallpaper {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--print-default-config") => {
            print!("{}", config::DEFAULT);
            return;
        }
        Some("--outputs") => {
            for name in wl::output_names().unwrap_or_else(|e| fail(e)) {
                println!("{name}");
            }
            return;
        }
        Some("--running") => std::process::exit(if lock().is_none() { 0 } else { 1 }),
        Some("--status") => status(),
        Some("set") => return set(&args[1..]),
        Some("thumbnail") => {
            // One line each, in order: the thumbnail, or empty if it can't be made.
            let mut failed = false;
            for f in &args[1..] {
                match thumb::get(std::path::Path::new(f)) {
                    Ok(t) => println!("{}", t.display()),
                    Err(e) => {
                        eprintln!("herowallpaper: {e}");
                        println!();
                        failed = true;
                    }
                }
            }
            std::process::exit(failed as i32)
        }
        Some(other) => fail(format!("unknown argument {other:?} (see --help)")),
    }
    let path = config::path().unwrap_or_else(|| fail("no home directory"));
    let Some(mut lock) = lock() else {
        fail("already running (change the picture with `herowallpaper set PICTURE`)");
    };
    // Held (locked) for as long as this runs; says who holds it.
    {
        use std::io::{Seek, Write};
        let _ = lock.set_len(0);
        let _ = lock.rewind();
        let _ = writeln!(lock, "{} {}", std::process::id(), env!("CARGO_PKG_VERSION"));
    }
    report(None);
    let _ = std::fs::remove_file(runtime_file("video"));
    let _ = std::fs::remove_file(runtime_file("preparing"));
    let _lock = lock;
    wl::run(path).unwrap_or_else(|e| fail(e));
}

/// `set PICTURE [--mode M] [--output NAME]`: writes the config, which the
/// running wallpaper follows.
fn set(args: &[String]) {
    let (mut picture, mut mode, mut output) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => mode = Some(it.next().cloned().unwrap_or_else(|| fail("--mode needs a value"))),
            "--output" => output = Some(it.next().cloned().unwrap_or_else(|| fail("--output needs a name"))),
            _ if picture.is_none() => picture = Some(a.clone()),
            _ => fail(format!("unexpected {a:?}")),
        }
    }
    let file = config::path().unwrap_or_else(|| fail("no home directory"));
    if let Some(m) = &mode {
        if config::Mode::parse(m).is_none() {
            fail(format!("unknown mode {m:?} (cover, contain, stretch, center or tile)"));
        }
        config::set(&file, output.as_deref(), "mode", m).unwrap_or_else(|e| fail(e));
    }
    if let Some(p) = picture {
        // Absolute, so the config doesn't depend on where this ran.
        let abs = if p.is_empty() {
            p
        } else {
            let p = PathBuf::from(p);
            if !p.exists() {
                fail(format!("{}: no such file", p.display()));
            }
            std::fs::canonicalize(&p).unwrap_or(p).to_string_lossy().into_owned()
        };
        config::set(&file, output.as_deref(), "path", &abs).unwrap_or_else(|e| fail(e));
    } else if mode.is_none() {
        fail("set what? (see --help)");
    }
}

/// A file of this session's wallpaper in $XDG_RUNTIME_DIR.
pub fn runtime_file(ext: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default().replace('/', "_");
    dir.join(format!("herowallpaper-{display}.{ext}"))
}

/// Records what went wrong last (None: nothing now), for `--status` and
/// settings apps, as well as on stderr.
pub fn report(problem: Option<&str>) {
    if let Some(p) = problem {
        eprintln!("herowallpaper: {p}");
    }
    let _ = std::fs::write(runtime_file("status"), problem.unwrap_or(""));
}

/// Says how videos play, for `--status`.
pub fn videos_play(how: &str) {
    if std::env::var_os("HEROWALLPAPER_DEBUG").is_some() {
        eprintln!("herowallpaper: videos play {how}");
    }
    let _ = std::fs::write(runtime_file("video"), how);
}

/// `--status`: "running VERSION PID" (then how videos play, once one has,
/// and a fitted copy being made) and the last problem on the next line;
/// or "not running".
fn status() -> ! {
    if lock().is_some() {
        println!("not running");
        std::process::exit(1);
    }
    let who = std::fs::read_to_string(runtime_file("lock")).unwrap_or_default();
    let mut w = who.split_whitespace();
    let (pid, version) = (w.next().unwrap_or("?"), w.next().unwrap_or("?"));
    let mut line = format!("running {version} {pid}");
    let video = std::fs::read_to_string(runtime_file("video")).unwrap_or_default();
    if !video.is_empty() {
        line += &format!(" - videos play {video}");
    }
    if let Ok(p) = std::fs::read_to_string(runtime_file("preparing")) {
        line += &format!(" - preparing a copy of {p} fitted to the screen (once; a still until then)");
    }
    println!("{line}");
    let problem = std::fs::read_to_string(runtime_file("status")).unwrap_or_default();
    if !problem.trim().is_empty() {
        println!("{}", problem.trim());
    }
    std::process::exit(0)
}

/// The session's lock in $XDG_RUNTIME_DIR, if no wallpaper holds it.
fn lock() -> Option<std::fs::File> {
    extern "C" {
        fn flock(fd: std::ffi::c_int, op: std::ffi::c_int) -> std::ffi::c_int;
    }
    const LOCK_EX: std::ffi::c_int = 2;
    const LOCK_NB: std::ffi::c_int = 4;
    // Not truncated: the running wallpaper's pid and version are in it.
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(runtime_file("lock")).ok()?;
    use std::os::fd::AsRawFd;
    (unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0).then_some(f)
}

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
mod picture;
mod watch;
mod wl;

use std::path::PathBuf;

fn usage() -> ! {
    println!(
        "herowallpaper {}

Usage:
  herowallpaper                  run (one per session)
  herowallpaper set PICTURE      show PICTURE (an image file; \"\" for none)
      [--mode cover|contain|stretch|center|tile] [--output NAME]
  herowallpaper --outputs        list the screens' names
  herowallpaper --print-default-config

Settings: ~/.config/hero/wallpaper.toml (applied when saved).",
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
        Some("set") => return set(&args[1..]),
        Some(other) => fail(format!("unknown argument {other:?} (see --help)")),
    }
    let path = config::path().unwrap_or_else(|| fail("no home directory"));
    if !single_instance() {
        fail("already running (change the picture with `herowallpaper set PICTURE`)");
    }
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

/// Holds a lock in $XDG_RUNTIME_DIR for as long as this runs.
fn single_instance() -> bool {
    extern "C" {
        fn flock(fd: std::ffi::c_int, op: std::ffi::c_int) -> std::ffi::c_int;
    }
    const LOCK_EX: std::ffi::c_int = 2;
    const LOCK_NB: std::ffi::c_int = 4;
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default().replace('/', "_");
    let Ok(f) = std::fs::File::create(dir.join(format!("herowallpaper-{display}.lock"))) else { return true };
    use std::os::fd::AsRawFd;
    if unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        return false;
    }
    // Kept open (and locked) until exit.
    std::mem::forget(f);
    true
}

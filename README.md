# HeroWallpaper

A lightweight wallpaper for Wayland, part of [HeroineOS](https://github.com/HeroineOS).
It's a standalone program: use it with HeroWM, or with sway, Hyprland, KDE, or any
compositor with wlr-layer-shell.

- **Still and animated pictures:** JPEG, PNG and APNG, GIF, WebP (animated too), AVIF,
  BMP, TIFF, QOI. Phone photos are turned upright (EXIF).
- **Looping videos:** MP4, MKV, WebM, MOV and anything else FFmpeg plays, silent, through
  the `ffmpeg` command (install it: `sudo apt install ffmpeg`; pictures don't need it).
- **Crossfades** when the picture changes, and on startup.
- **A picture per screen**, or one for all; screens plugged in later get theirs.
- **Fill modes:** cover, contain, stretch, center, tile.
- **Follows the theme:** the background color behind the picture is the HeroUI theme's
  unless you set one. When the theme's animations are off (battery saver), fades are off
  and animated pictures hold still on their first frame, with their frames freed.
- **Light:** the compositor does the scaling and fading (wp_viewporter,
  wp_alpha_modifier), so between changes the process just sleeps. Measured on sway:
  - **Memory:** about 0.3 MB of its own memory (5 MB RSS with shared libraries), one
    thread. A 24-megapixel JPEG peaks at 18 MB while loading, because JPEGs are decoded
    at the fraction of their size the screen needs.
  - **CPU:** 0% for a still picture. About 0.2% playing a 15 fps GIF. No wakeups at
    all while a window covers it, since animations pause when the compositor stops
    drawing the wallpaper.
  - **Pixels:** never more than the screen shows, held in memory shared with the
    compositor. Pictures smaller than the screen are kept at their own size and scaled
    up by the GPU.

## Install

Debian packages for amd64 and arm64 are attached to the
[releases](https://github.com/HeroineOS/HeroWallpaper/releases):

```sh
sudo apt install ./herowallpaper_*_arm64.deb
herowallpaper set ~/Pictures/wallpaper.jpg
herowallpaper &
```

Start `herowallpaper` with your session; in HeroWM's config:

```toml
autostart = ["herowallpaper"]
```

## Use

```sh
herowallpaper set PICTURE                     # all screens
herowallpaper set PICTURE --mode contain      # cover, contain, stretch, center, tile
herowallpaper set PICTURE --output HDMI-A-1   # one screen (names: herowallpaper --outputs)
herowallpaper set --mode tile                 # just the mode
herowallpaper set ""                          # no picture: the background color
herowallpaper --running                       # exit status 0 if it's running
herowallpaper thumbnail FILE...               # 256 px thumbnails (shared freedesktop cache)
```

HeroAppearance's Wallpaper page does all of this with a gallery.

`set` edits `~/.config/hero/wallpaper.toml` (keeping your comments), and the running
wallpaper follows the file as soon as it's saved, whoever saves it.
`herowallpaper --print-default-config` prints a commented starting point:

```toml
path = "~/Pictures/wallpaper.jpg"
mode = "cover"
# color = "#1e1e2e"       # unset: the theme's background
transition = 450          # crossfade, ms (0: none)
animate = true            # false: animated pictures show their first frame
animation_memory = 256    # MB an animation's frames may take

[output."HDMI-A-1"]
path = "~/Pictures/other.png"
mode = "contain"
```

Animated pictures keep every frame decoded (at no more than the screen's size), which
is what makes them cost nothing to play. One whose frames would take more than
`animation_memory` shows its first frame instead. Long or large animations will be
better as videos, which are planned.

## Videos

Linux has no video decoding of its own (the kernel only drives hardware decoders), so
videos need a media library; FFmpeg is the one every media player sits on. HeroWallpaper
runs the plain `ffmpeg` command rather than a player like mpv (much bigger) or FFmpeg's
libraries (their versions differ between distro releases, which would tie the package to
one). ffmpeg decodes with hardware where there is some (`-hwaccel auto`), crops and
scales to exactly the pixels the screen shows, and hands raw frames over a pipe; they go
into shared memory the compositor reads. Frames are only read while the compositor draws
the wallpaper, so ffmpeg waits whenever windows cover it.

Measured on sway (a VM without a GPU, 1080p H.264 video on a 900×700 screen, software
decoding): ffmpeg about 45% of one core and 98 MB, HeroWallpaper about 8% (copying
frames) and 0.3 MB of its own memory. **Covered by a window: 0% for both.** With the
theme's animations off (battery saver) or `animate = false`, ffmpeg runs once for the
first frame and exits. A video wallpaper always costs more than a picture: a short
looping animation as an animated WebP or APNG costs almost nothing to play.

## Planned

- Streams and links (ffmpeg plays URLs; YouTube through yt-dlp).
- Lighter video: frames handed to the compositor as GPU buffers (no copies) where the
  hardware decodes.
- Animated AVIF and JPEG XL.

## Building

```sh
sudo apt install build-essential pkg-config libdav1d-dev
cargo build --release
```

Pure-Rust Wayland client, so libwayland isn't needed; libdav1d decodes AVIF. It builds
for x86_64, arm64, i686 and armv7.

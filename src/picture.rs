//! Decoding a picture and fitting it to a screen. The pixels go straight
//! into shared memory the compositor reads (one frame, or every frame of
//! an animated picture), never larger than the screen shows them:
//! pictures bigger than the screen are scaled down here (JPEGs decoded
//! at a fraction of their size to begin with), smaller ones are scaled up
//! by the compositor as it draws them. The decoded image is gone once
//! that's written.

use std::fs::File;
use std::io::{BufReader, Write};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::time::Duration;

use fast_image_resize as fr;
use image::{AnimationDecoder, DynamicImage, ImageDecoder};

use crate::config::Mode;

/// Where a picture goes on a screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    /// The part of the picture shown (pixels: x, y, w, h).
    pub crop: (f64, f64, f64, f64),
    /// The pixels kept for it.
    pub buf: (u32, u32),
    /// Where they go on the screen (logical: x, y, w, h).
    pub dest: (i32, i32, i32, i32),
}

/// Fits a `w`×`h` picture on a screen of `logical` size at `scale`.
pub fn fit(mode: Mode, (w, h): (u32, u32), logical: (i32, i32), scale: f64) -> Fit {
    let (lw, lh) = (logical.0.max(1), logical.1.max(1));
    let (pw, ph) = ((lw as f64 * scale).round().max(1.0), (lh as f64 * scale).round().max(1.0));
    let (iw, ih) = (w.max(1) as f64, h.max(1) as f64);
    let size = |v: f64| v.round().max(1.0) as u32;
    // Logical size of `p` physical pixels.
    let lsize = |p: f64| (p / scale).round().max(1.0) as i32;
    match mode {
        Mode::Cover => {
            // The middle of the picture, in the screen's proportions.
            let (cw, ch) = if iw / ih > pw / ph { (ih * pw / ph, ih) } else { (iw, iw * ph / pw) };
            let k = (pw / cw).min(1.0);
            Fit { crop: ((iw - cw) / 2.0, (ih - ch) / 2.0, cw, ch), buf: (size(cw * k), size(ch * k)), dest: (0, 0, lw, lh) }
        }
        Mode::Contain => {
            let s = (pw / iw).min(ph / ih);
            let (dw, dh) = (lsize(iw * s), lsize(ih * s));
            let k = s.min(1.0);
            Fit { crop: (0.0, 0.0, iw, ih), buf: (size(iw * k), size(ih * k)), dest: ((lw - dw) / 2, (lh - dh) / 2, dw, dh) }
        }
        Mode::Stretch => Fit { crop: (0.0, 0.0, iw, ih), buf: (size(iw.min(pw)), size(ih.min(ph))), dest: (0, 0, lw, lh) },
        Mode::Center => {
            let (cw, ch) = (iw.min(pw), ih.min(ph));
            let (dw, dh) = (lsize(cw), lsize(ch));
            Fit { crop: ((iw - cw) / 2.0, (ih - ch) / 2.0, cw, ch), buf: (size(cw), size(ch)), dest: ((lw - dw) / 2, (lh - dh) / 2, dw, dh) }
        }
        // Built at the screen's size from pictures at actual size.
        Mode::Tile => Fit { crop: (0.0, 0.0, iw, ih), buf: (size(pw), size(ph)), dest: (0, 0, lw, lh) },
    }
}

/// A picture ready to show: its frames one after another in `memory`,
/// each `size.0 * size.1 * 4` bytes (wl_shm ARGB/XRGB8888).
pub struct Loaded {
    pub memory: File,
    pub size: (u32, u32),
    pub opaque: bool,
    /// How long each frame shows (one frame: still).
    pub delays: Vec<Duration>,
    pub dest: (i32, i32, i32, i32),
}

impl Loaded {
    pub fn frame_bytes(&self) -> usize {
        self.size.0 as usize * self.size.1 as usize * 4
    }
}

/// What to load and for which screen.
pub struct Request<'a> {
    pub path: &'a Path,
    pub mode: Mode,
    pub logical: (i32, i32),
    pub scale: f64,
    /// Play animated pictures (else their first frame).
    pub animate: bool,
    /// Bytes all of an animation's frames may take.
    pub budget: usize,
}

pub fn load(r: &Request) -> Result<Loaded, String> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", r.path.display());
    let file = || File::open(r.path).map(BufReader::new).map_err(|e| err(&e));
    let format = image::ImageReader::new(file()?).with_guessed_format().map_err(|e| err(&e))?.format();
    use image::ImageFormat as F;
    // Animated pictures, when they are.
    if r.animate {
        let frames: Option<image::Frames> = match format {
            Some(F::Gif) => Some(image::codecs::gif::GifDecoder::new(file()?).map_err(|e| err(&e))?.into_frames()),
            Some(F::Png) => {
                let d = image::codecs::png::PngDecoder::new(file()?).map_err(|e| err(&e))?;
                if d.is_apng().unwrap_or(false) { Some(d.apng().map_err(|e| err(&e))?.into_frames()) } else { None }
            }
            Some(F::WebP) => {
                let d = image::codecs::webp::WebPDecoder::new(file()?).map_err(|e| err(&e))?;
                if d.has_animation() { Some(d.into_frames()) } else { None }
            }
            _ => None,
        };
        if let Some(frames) = frames {
            match animation(r, frames) {
                Ok(Some(l)) => return Ok(l),
                Ok(None) => eprintln!("herowallpaper: {}: its frames would take more than animation_memory; showing the first", r.path.display()),
                Err(e) => return Err(err(&e)),
            }
        }
    }
    let img = if format == Some(F::Jpeg) {
        jpeg(r).map_err(|e| err(&e))?
    } else {
        let mut d = image::ImageReader::new(file()?).with_guessed_format().map_err(|e| err(&e))?.into_decoder().map_err(|e| err(&e))?;
        let o = d.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut img = DynamicImage::from_decoder(d).map_err(|e| err(&e))?;
        img.apply_orientation(o);
        img
    };
    let f = fit(r.mode, (img.width(), img.height()), r.logical, r.scale);
    let (pixels, opaque) = render(&img, &f, r.mode, fr::FilterType::Lanczos3).map_err(|e| err(&e))?;
    drop(img);
    let mut memory = memfd().map_err(|e| err(&e))?;
    memory.write_all(&pixels).map_err(|e| err(&e))?;
    Ok(Loaded { memory, size: f.buf, opaque, delays: vec![Duration::ZERO], dest: f.dest })
}

/// Every frame, if they fit the budget.
fn animation(r: &Request, frames: image::Frames) -> Result<Option<Loaded>, image::ImageError> {
    let mut memory = memfd()?;
    let (mut size, mut dest, mut opaque, mut delays) = ((0, 0), (0, 0, 0, 0), true, vec![]);
    let mut total = 0usize;
    for frame in frames {
        let frame = frame?;
        let (n, d) = frame.delay().numer_denom_ms();
        // Like browsers: no delay (or a tiny one) means a tenth of a second.
        let ms = if d == 0 { 100 } else { n / d };
        delays.push(Duration::from_millis(if ms <= 10 { 100 } else { ms as u64 }));
        let img = DynamicImage::ImageRgba8(frame.into_buffer());
        let f = fit(r.mode, (img.width(), img.height()), r.logical, r.scale);
        // Frames are the same size; the first one decides.
        if delays.len() == 1 {
            (size, dest) = (f.buf, f.dest);
        }
        total += size.0 as usize * size.1 as usize * 4;
        if total > r.budget {
            return Ok(None);
        }
        let (pixels, o) = render(&img, &f, r.mode, fr::FilterType::Bilinear).map_err(|e| image::ImageError::IoError(std::io::Error::other(e)))?;
        opaque &= o;
        memory.write_all(&pixels)?;
    }
    if delays.is_empty() {
        return Err(image::ImageError::IoError(std::io::Error::other("no frames")));
    }
    Ok(Some(Loaded { memory, size, opaque, delays, dest }))
}

/// A JPEG, decoded at the smallest fraction of its size (1/8 to 1) that's
/// still as large as the screen shows it: a 24 MP photo for a 1080p
/// screen decodes at 1/2 or 1/4, a quarter or a sixteenth of the memory.
fn jpeg(r: &Request) -> Result<DynamicImage, String> {
    let mut d = jpeg_decoder::Decoder::new(BufReader::new(File::open(r.path).map_err(|e| e.to_string())?));
    d.read_info().map_err(|e| e.to_string())?;
    let info = d.info().ok_or("no size")?;
    let o = d.exif_data().map_or(1, exif_orientation);
    let turned = (5..=8).contains(&o);
    let (w, h) = (info.width as u32, info.height as u32);
    let (ow, oh) = if turned { (h, w) } else { (w, h) };
    let f = fit(r.mode, (ow, oh), r.logical, r.scale);
    let k = if r.mode == Mode::Tile { 1.0 } else { (f.buf.0 as f64 / f.crop.2).max(f.buf.1 as f64 / f.crop.3).min(1.0) };
    let (want_w, want_h) = ((w as f64 * k).ceil() as u16, (h as f64 * k).ceil() as u16);
    d.scale(want_w.max(1), want_h.max(1)).map_err(|e| e.to_string())?;
    let px = d.decode().map_err(|e| e.to_string())?;
    let info = d.info().ok_or("no size")?;
    let (w, h) = (info.width as u32, info.height as u32);
    use jpeg_decoder::PixelFormat as P;
    let img = match info.pixel_format {
        P::RGB24 => image::RgbImage::from_raw(w, h, px).map(DynamicImage::ImageRgb8),
        P::L8 => image::GrayImage::from_raw(w, h, px).map(DynamicImage::ImageLuma8),
        P::L16 => {
            let v = px.chunks_exact(2).map(|c| c[0]).collect();
            image::GrayImage::from_raw(w, h, v).map(DynamicImage::ImageLuma8)
        }
        P::CMYK32 => {
            // As Adobe writes them (inverted).
            let v = px.chunks_exact(4).flat_map(|c| { let k = c[3] as u32; [c[0], c[1], c[2]].map(|v| (v as u32 * k / 255) as u8) }).collect();
            image::RgbImage::from_raw(w, h, v).map(DynamicImage::ImageRgb8)
        }
    };
    let mut img = img.ok_or("bad pixel data")?;
    if let Some(o) = image::metadata::Orientation::from_exif(o) {
        img.apply_orientation(o);
    }
    Ok(img)
}

/// The EXIF orientation (1: as stored).
fn exif_orientation(d: &[u8]) -> u8 {
    let d = d.strip_prefix(b"Exif\0\0").unwrap_or(d);
    let le = d.starts_with(b"II");
    let u16_at = |o: usize| d.get(o..o + 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) });
    let u32_at = |o: usize| d.get(o..o + 4).map(|b| if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) });
    let Some(ifd) = u32_at(4).map(|v| v as usize) else { return 1 };
    let n = u16_at(ifd).unwrap_or(0) as usize;
    (0..n)
        .map(|i| ifd + 2 + i * 12)
        .find(|&e| u16_at(e) == Some(0x0112))
        .and_then(|e| u16_at(e + 8))
        .map_or(1, |v| if (1..=8).contains(&v) { v as u8 } else { 1 })
}

/// The pixels for `fit` (BGRA, alpha premultiplied, as wl_shm wants), and
/// whether they're all opaque.
fn render(img: &DynamicImage, fit: &Fit, mode: Mode, filter: fr::FilterType) -> Result<(Vec<u8>, bool), String> {
    let alpha = img.color().has_alpha();
    let (src, pt) = if alpha { (img.to_rgba8().into_raw(), fr::PixelType::U8x4) } else { (img.to_rgb8().into_raw(), fr::PixelType::U8x3) };
    let (w, h) = (img.width(), img.height());
    let (bw, bh) = fit.buf;
    let n = pt.size();
    let scaled = if mode == Mode::Tile {
        // At actual size, repeated over the screen.
        let mut out = vec![0u8; bw as usize * bh as usize * n];
        for y in 0..bh as usize {
            let row = &src[(y % h as usize) * w as usize * n..][..w as usize * n];
            let dst = &mut out[y * bw as usize * n..][..bw as usize * n];
            for chunk in dst.chunks_mut(w as usize * n) {
                chunk.copy_from_slice(&row[..chunk.len()]);
            }
        }
        out
    } else {
        let src_img = fr::images::ImageRef::new(w, h, &src, pt).map_err(|e| e.to_string())?;
        let mut dst = fr::images::Image::new(bw, bh, pt);
        let (x, y, cw, ch) = fit.crop;
        let opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(filter)).crop(x, y, cw, ch);
        fr::Resizer::new().resize(&src_img, &mut dst, &opts).map_err(|e| e.to_string())?;
        dst.into_vec()
    };
    drop(src);
    let mut opaque = true;
    let out = if alpha {
        let mut v = scaled;
        for p in v.chunks_exact_mut(4) {
            let a = p[3] as u32;
            opaque &= a == 255;
            let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
            p.copy_from_slice(&[m(p[2]), m(p[1]), m(p[0]), p[3]]);
        }
        v
    } else {
        let mut v = Vec::with_capacity(bw as usize * bh as usize * 4);
        for p in scaled.chunks_exact(3) {
            v.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        v
    };
    Ok((out, opaque))
}

/// Memory to share with the compositor (written, never mapped here).
pub fn memfd() -> std::io::Result<File> {
    extern "C" {
        fn memfd_create(name: *const std::ffi::c_char, flags: std::ffi::c_uint) -> std::ffi::c_int;
    }
    const MFD_CLOEXEC: std::ffi::c_uint = 1;
    let fd = unsafe { memfd_create(c"herowallpaper".as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_crops_and_never_upscales() {
        // A wide 4000x1000 picture on a 1920x1080 screen at scale 1.
        let f = fit(Mode::Cover, (4000, 1000), (1920, 1080), 1.0);
        assert_eq!(f.dest, (0, 0, 1920, 1080));
        assert!((f.crop.2 - 1777.8).abs() < 0.1 && f.crop.3 == 1000.0, "{f:?}");
        // Smaller than the screen: kept as is, the compositor scales it.
        assert_eq!(f.buf, (1778, 1000));
        // Bigger: scaled down to the screen's pixels (scale 1.5).
        let f = fit(Mode::Cover, (6000, 4000), (1280, 720), 1.5);
        assert_eq!(f.buf, (1920, 1080));
    }

    #[test]
    fn contain_and_center() {
        let f = fit(Mode::Contain, (1000, 1000), (1920, 1080), 1.0);
        assert_eq!(f.dest, (420, 0, 1080, 1080));
        assert_eq!(f.buf, (1000, 1000));
        let f = fit(Mode::Center, (3000, 500), (1920, 1080), 2.0);
        assert_eq!(f.buf, (3000, 500));
        assert_eq!(f.dest, (210, 415, 1500, 250));
    }

    #[test]
    fn exif() {
        // Little-endian TIFF, one IFD entry: orientation 6.
        let mut d = b"Exif\0\0II*\0\x08\0\0\0\x01\0".to_vec();
        d.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        assert_eq!(exif_orientation(&d), 6);
        assert_eq!(exif_orientation(b"junk"), 1);
    }

    fn req(path: &Path, mode: Mode) -> Request<'_> {
        Request { path, mode, logical: (32, 16), scale: 1.0, animate: true, budget: 1 << 20 }
    }

    #[test]
    fn loads_still_and_animated() {
        let dir = std::env::temp_dir().join(format!("herowallpaper-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A 64x32 PNG with see-through pixels.
        let png = dir.join("a.png");
        image::RgbaImage::from_fn(64, 32, |x, _| image::Rgba([200, 100, 50, if x < 32 { 255 } else { 128 }])).save(&png).unwrap();
        let l = load(&req(&png, Mode::Stretch)).unwrap();
        assert_eq!((l.size, l.opaque, l.delays.len()), ((32, 16), false, 1));
        let mut px = vec![];
        use std::io::{Read, Seek};
        let mut m = l.memory;
        m.rewind().unwrap();
        m.read_to_end(&mut px).unwrap();
        assert_eq!(px.len(), 32 * 16 * 4);
        // BGRA: the left (opaque) column.
        assert_eq!(&px[..4], &[50, 100, 200, 255]);
        // A 3-frame GIF.
        let gif = dir.join("b.gif");
        {
            let mut enc = image::codecs::gif::GifEncoder::new(File::create(&gif).unwrap());
            for c in [0u8, 120, 240] {
                let f = image::Frame::from_parts(image::RgbaImage::from_pixel(8, 8, image::Rgba([c, c, c, 255])), 0, 0, image::Delay::from_numer_denom_ms(50, 1));
                enc.encode_frame(f).unwrap();
            }
        }
        let l = load(&req(&gif, Mode::Cover)).unwrap();
        assert_eq!(l.delays, vec![Duration::from_millis(50); 3]);
        assert_eq!(l.size, (8, 4));
        // Over budget: the first frame only.
        let l = load(&Request { budget: 8 * 4 * 4, ..req(&gif, Mode::Cover) }).unwrap();
        assert_eq!(l.delays.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

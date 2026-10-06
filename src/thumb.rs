//! Thumbnails in the shared freedesktop cache
//! (`~/.cache/thumbnails/large`, 256 px), so settings apps can show
//! pictures without decoding them at full size, and file managers reuse
//! them (and we theirs).

use std::path::{Path, PathBuf};

const SIZE: u32 = 256;

fn cache_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|c| c.join("thumbnails").join("large"))
}

/// `file://` URI of an absolute path, as the spec hashes it.
fn uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_encoded_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn mtime(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// The thumbnail's recorded modification time of its picture.
fn recorded_mtime(thumb: &Path) -> Option<u64> {
    let d = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(thumb).ok()?));
    let r = d.read_info().ok()?;
    let info = r.info();
    info.uncompressed_latin1_text.iter().find(|t| t.keyword == "Thumb::MTime").and_then(|t| t.text.parse().ok())
}

/// A thumbnail of `picture`: the cached one if still current, else a new one.
pub fn get(picture: &Path) -> Result<PathBuf, String> {
    let picture = std::fs::canonicalize(picture).map_err(|e| format!("{}: {e}", picture.display()))?;
    let dir = cache_dir().ok_or("no home directory")?;
    let uri = uri(&picture);
    let thumb = dir.join(format!("{:x}.png", md5::compute(uri.as_bytes())));
    let mtime = mtime(&picture).unwrap_or(0);
    if recorded_mtime(&thumb) == Some(mtime) {
        return Ok(thumb);
    }
    let img = if crate::video::is_video(&picture) { crate::video::still(&picture, SIZE)? } else { crate::picture::small(&picture, SIZE)? };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Written whole, then moved into place (others may read the cache).
    let tmp = dir.join(format!(".herowallpaper-{}.png", std::process::id()));
    let write = || -> Result<(), png::EncodingError> {
        let mut e = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(&tmp)?), img.width(), img.height());
        e.set_color(png::ColorType::Rgba);
        e.add_text_chunk("Thumb::URI".into(), uri.clone())?;
        e.add_text_chunk("Thumb::MTime".into(), mtime.to_string())?;
        e.write_header()?.write_image_data(img.as_raw())?;
        Ok(())
    };
    write().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &thumb).map_err(|e| e.to_string())?;
    Ok(thumb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_escapes_like_the_spec() {
        assert_eq!(uri(Path::new("/home/a b/ü.png")), "file:///home/a%20b/%C3%BC.png");
    }

    #[test]
    fn makes_and_reuses() {
        let dir = std::env::temp_dir().join(format!("herowallpaper-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        let pic = dir.join("p.png");
        image::RgbImage::from_pixel(1000, 500, image::Rgb([1, 2, 3])).save(&pic).unwrap();
        let t = get(&pic).unwrap();
        let made = std::fs::metadata(&t).unwrap().modified().unwrap();
        assert_eq!(image::image_dimensions(&t).unwrap(), (256, 128));
        assert_eq!(get(&pic).unwrap(), t);
        assert_eq!(std::fs::metadata(&t).unwrap().modified().unwrap(), made, "remade");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

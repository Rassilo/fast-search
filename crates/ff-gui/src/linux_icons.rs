//! Linux and other free desktops: the icons the file manager shows, from
//! the desktop's icon theme (freedesktop Icon Theme and Shared MIME-info
//! specifications). A file's type comes from its name, by the MIME
//! database's patterns, so no file is opened. PNG icons are decoded here;
//! SVG ones (most of Adwaita and Breeze) are converted once by the
//! system's `rsvg-convert` and kept in ~/.cache/ff/icons. Without a theme
//! or that tool, the built-in icons are used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One folder of a theme: where its icons are and the sizes they suit.
struct Dir {
    path: String,
    size: u32,
    min: u32,
    max: u32,
}

struct Theme {
    name: String,
    dirs: Vec<Dir>,
}

pub struct Themes {
    /// The base folders themes live in: ~/.icons, then the XDG data ones.
    roots: Vec<PathBuf>,
    /// The chosen theme, then those it inherits from, ending with hicolor.
    chain: Vec<Theme>,
    /// File name patterns ("pdf", "tar.gz") to MIME types.
    globs: HashMap<String, String>,
    /// MIME types to their generic icon ("package-x-generic").
    generic: HashMap<String, String>,
    cache: Option<PathBuf>,
}

impl Themes {
    pub fn load() -> Themes {
        let roots = icon_roots();
        let mut chain: Vec<Theme> = Vec::new();
        let mut queue = vec![theme_name()];
        while let Some(name) = queue.pop() {
            if chain.iter().any(|t| t.name == name) || chain.len() > 12 {
                continue;
            }
            let Some((theme, inherits)) = read_theme(&roots, &name) else { continue };
            chain.push(theme);
            // Inherited themes in their order (the stack pops the first).
            queue.extend(inherits.into_iter().rev());
        }
        if !chain.iter().any(|t| t.name == "hicolor") {
            chain.extend(read_theme(&roots, "hicolor").map(|t| t.0));
        }
        let mime = data_dirs().into_iter().map(|d| d.join("mime")).collect::<Vec<_>>();
        let mut globs = HashMap::new();
        let mut generic = HashMap::new();
        for dir in mime.iter().rev() {
            // weight:type:pattern, highest weight first: keep the first.
            if let Ok(text) = std::fs::read_to_string(dir.join("globs2")) {
                for line in text.lines().filter(|l| !l.starts_with('#')) {
                    let mut parts = line.splitn(4, ':');
                    let (_, Some(mime), Some(pattern)) = (parts.next(), parts.next(), parts.next()) else { continue };
                    if let Some(ext) = pattern.strip_prefix("*.").filter(|e| !e.contains(['*', '?', '['])) {
                        globs.entry(ext.to_ascii_lowercase()).or_insert_with(|| mime.to_string());
                    }
                }
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("generic-icons")) {
                for (mime, icon) in text.lines().filter_map(|l| l.split_once(':')) {
                    generic.insert(mime.to_string(), icon.to_string());
                }
            }
        }
        let cache = ff_core::os::cache_dir().map(|c| c.join("ff").join("icons"));
        Themes { roots, chain, globs, generic, cache }
    }

    /// True if there is a theme to take icons from.
    pub fn usable(&self) -> bool {
        !self.chain.is_empty()
    }

    /// The icon for an entry, as `size` x `size` BGRA pixels: by type for
    /// a file, the folder icon, or a drive's for a whole drive.
    pub fn icon(&self, name: &[u8], is_dir: bool, drive: bool, size: usize) -> Option<Vec<[u8; 4]>> {
        let names: Vec<String> = if drive {
            vec!["drive-harddisk".into(), "folder".into()]
        } else if is_dir {
            vec!["folder".into()]
        } else {
            let mut names = Vec::new();
            if let Some(mime) = self.mime_of(name) {
                names.push(mime.replace('/', "-"));
                if let Some(generic) = self.generic.get(&mime) {
                    names.push(generic.clone());
                }
                let major = mime.split('/').next().unwrap_or("");
                names.push(format!("{major}-x-generic"));
            }
            names.extend(["text-x-generic".to_string(), "unknown".to_string()]);
            names
        };
        names.iter().find_map(|n| self.find(n, size)).and_then(|file| self.read(&file, size))
    }

    /// The MIME type of a file name, by its longest known extension.
    fn mime_of(&self, name: &[u8]) -> Option<String> {
        let name = String::from_utf8_lossy(name).to_ascii_lowercase();
        let mut at = 0;
        while let Some(dot) = name[at..].find('.') {
            let ext = &name[at + dot + 1..];
            if let Some(mime) = self.globs.get(ext) {
                return Some(mime.clone());
            }
            at += dot + 1;
        }
        None
    }

    /// The file of icon `name` closest to `size`, in the first theme of
    /// the chain that has it.
    fn find(&self, name: &str, size: usize) -> Option<PathBuf> {
        let size = size as u32;
        for theme in &self.chain {
            let mut best: Option<(u32, PathBuf)> = None;
            for dir in &theme.dirs {
                let distance = if size < dir.min { dir.min - size } else { size.saturating_sub(dir.max) };
                if best.as_ref().is_some_and(|(d, _)| *d <= distance) {
                    continue;
                }
                for root in &self.roots {
                    let base = root.join(&theme.name).join(&dir.path);
                    if let Some(file) = ["png", "svg"].iter().map(|ext| base.join(format!("{name}.{ext}"))).find(|f| f.exists()) {
                        best = Some((distance, file));
                        break;
                    }
                }
                if best.as_ref().is_some_and(|(d, _)| *d == 0) {
                    break;
                }
            }
            if let Some((_, file)) = best {
                return Some(file);
            }
        }
        None
    }

    /// Reads an icon file at `size`: a PNG directly, an SVG through
    /// rsvg-convert (once; the result is kept in the cache folder).
    fn read(&self, file: &Path, size: usize) -> Option<Vec<[u8; 4]>> {
        let png = if file.extension().is_some_and(|e| e == "svg") {
            let cache = self.cache.as_ref()?;
            let key = format!("{:016x}-{size}.png", hash(file.as_os_str().as_encoded_bytes()));
            let out = cache.join(key);
            if !out.exists() {
                std::fs::create_dir_all(cache).ok()?;
                let made = std::process::Command::new("rsvg-convert")
                    .args(["-w", &size.to_string(), "-h", &size.to_string(), "-o"])
                    .arg(&out)
                    .arg(file)
                    .stderr(std::process::Stdio::null())
                    .status()
                    .ok()?;
                if !made.success() {
                    return None;
                }
            }
            std::fs::read(out).ok()?
        } else {
            std::fs::read(file).ok()?
        };
        let image = crate::png::decode(&png)?;
        Some(scale(&image, size))
    }
}

/// Resizes to `size` x `size`: each target pixel averages the source
/// pixels under it (weighted by their opacity, so edges do not darken).
fn scale(image: &crate::png::Image, size: usize) -> Vec<[u8; 4]> {
    if image.w == size && image.h == size {
        return image.px.clone();
    }
    let mut out = Vec::with_capacity(size * size);
    for y in 0..size {
        let (y0, y1) = (y * image.h / size, ((y + 1) * image.h / size).max(y * image.h / size + 1).min(image.h));
        for x in 0..size {
            let (x0, x1) = (x * image.w / size, ((x + 1) * image.w / size).max(x * image.w / size + 1).min(image.w));
            let (mut sum, mut alpha, mut n) = ([0u32; 3], 0u32, 0u32);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = image.px[sy * image.w + sx];
                    let a = p[3] as u32;
                    for k in 0..3 {
                        sum[k] += p[k] as u32 * a;
                    }
                    alpha += a;
                    n += 1;
                }
            }
            let c = |k: usize| if alpha == 0 { 0 } else { (sum[k] / alpha) as u8 };
            out.push([c(0), c(1), c(2), (alpha / n.max(1)) as u8]);
        }
    }
    out
}

fn hash(bytes: &[u8]) -> u64 {
    // FNV-1a: enough to name cache files.
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn data_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| ff_core::os::home_dir().map(|h| h.join(".local/share")))
        .into_iter()
        .collect();
    let system = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs
}

fn icon_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ff_core::os::home_dir().map(|h| h.join(".icons")).into_iter().collect();
    roots.extend(data_dirs().into_iter().map(|d| d.join("icons")));
    roots.retain(|r| r.is_dir());
    roots
}

/// The desktop's icon theme: FF_ICON_THEME, else GNOME's (gsettings),
/// else KDE's (kdeglobals), else Adwaita.
fn theme_name() -> String {
    if let Some(name) = std::env::var("FF_ICON_THEME").ok().filter(|v| !v.is_empty()) {
        return name;
    }
    let gnome = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_matches('\'').to_string())
        .filter(|n| !n.is_empty());
    if let Some(name) = gnome {
        return name;
    }
    let kde = ff_core::os::config_dir().and_then(|c| std::fs::read_to_string(c.join("kdeglobals")).ok()).and_then(|text| {
        let section = text.split("[Icons]").nth(1)?;
        let line = section.lines().take_while(|l| !l.starts_with('[')).find_map(|l| l.strip_prefix("Theme="))?;
        Some(line.trim().to_string())
    });
    kde.unwrap_or_else(|| "Adwaita".into())
}

/// A theme's folders (scale 1 only), and the themes it inherits from.
fn read_theme(roots: &[PathBuf], name: &str) -> Option<(Theme, Vec<String>)> {
    let text = roots.iter().find_map(|r| std::fs::read_to_string(r.join(name).join("index.theme")).ok())?;
    let mut sections: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    let mut current = "";
    for line in text.lines().map(str::trim) {
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = section;
        } else if let Some((key, value)) = line.split_once('=') {
            sections.entry(current).or_default().insert(key.trim(), value.trim());
        }
    }
    let head = sections.get("Icon Theme")?;
    let list = |key: &str| head.get(key).map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>()).unwrap_or_default();
    let mut dirs = Vec::new();
    for path in list("Directories") {
        let Some(info) = sections.get(path) else { continue };
        let num = |key: &str| info.get(key).and_then(|v| v.parse::<u32>().ok());
        let size = num("Size").unwrap_or(0);
        if size == 0 || num("Scale").unwrap_or(1) != 1 {
            continue;
        }
        let threshold = num("Threshold").unwrap_or(2);
        let (min, max) = match info.get("Type").copied().unwrap_or("Threshold") {
            "Fixed" => (size, size),
            "Scalable" => (num("MinSize").unwrap_or(size), num("MaxSize").unwrap_or(size)),
            _ => (size.saturating_sub(threshold), size + threshold),
        };
        dirs.push(Dir { path: path.to_string(), size, min, max });
    }
    // Exact sizes first, then the nearest, so a search can stop early.
    dirs.sort_by_key(|d| (d.max - d.min, d.size));
    let inherits = list("Inherits").into_iter().map(String::from).collect();
    Some((Theme { name: name.to_string(), dirs }, inherits))
}

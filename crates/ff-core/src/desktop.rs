//! Desktop actions on a found file: open it, show it in the file manager,
//! read its properties. Shared by the CLI and the GUI.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

fn succeeds(cmd: &mut Command) -> io::Result<bool> {
    let status = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    Ok(status.success())
}

/// Opens `path` with its default application (via `xdg-open`).
pub fn open(path: &Path) -> io::Result<()> {
    match succeeds(Command::new("xdg-open").arg(path)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(io::Error::other(format!("xdg-open could not open {}", path.display()))),
        Err(e) => Err(io::Error::new(e.kind(), format!("cannot run xdg-open: {e}"))),
    }
}

/// Opens `path` in the desktop's default text editor, whatever its type
/// (useful for scripts, HTML, JSON and other files whose normal "open"
/// runs them or shows them in another program). Falls back to a plain
/// open when the editor cannot be determined.
pub fn open_in_editor(path: &Path) -> io::Result<()> {
    let editor = Command::new("xdg-mime").args(["query", "default", "text/plain"]).output();
    let editor = editor.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    // gtk-launch starts a desktop entry by id with the file as argument.
    if !editor.is_empty() && succeeds(Command::new("gtk-launch").arg(&editor).arg(path)).unwrap_or(false) {
        return Ok(());
    }
    open(path)
}

/// `path` as a `file://` URI.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Shows `path` selected in the file manager. Falls back to opening its
/// folder when no file manager answers on D-Bus.
pub fn reveal(path: &Path) -> io::Result<()> {
    let uri = file_uri(path);
    let shown = succeeds(Command::new("gdbus").args([
        "call",
        "--session",
        "--dest=org.freedesktop.FileManager1",
        "--object-path=/org/freedesktop/FileManager1",
        "--method=org.freedesktop.FileManager1.ShowItems",
        &format!("['{uri}']"),
        "''",
    ]));
    if shown.unwrap_or(false) {
        return Ok(());
    }
    open(path.parent().unwrap_or(path))
}

/// Reads the current properties of `path` from disk as label/value pairs.
pub fn properties(path: &Path) -> io::Result<Vec<(&'static str, String)>> {
    let md = fs::symlink_metadata(path)?;
    let ft = md.file_type();
    let kind = if ft.is_dir() {
        "Folder"
    } else if ft.is_symlink() {
        "Symbolic link"
    } else if ft.is_socket() {
        "Socket"
    } else if ft.is_fifo() {
        "Named pipe"
    } else if ft.is_block_device() || ft.is_char_device() {
        "Device"
    } else {
        "File"
    };
    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();
    let mut out = vec![("Name", name), ("Type", kind.to_string())];
    if let Some(parent) = path.parent() {
        out.push(("Location", parent.display().to_string()));
    }
    if ft.is_symlink() {
        if let Ok(target) = fs::read_link(path) {
            out.push(("Target", target.display().to_string()));
        }
    }
    if ft.is_dir() {
        if let Ok(rd) = fs::read_dir(path) {
            out.push(("Contains", format!("{} items", crate::group_digits(rd.count() as u64))));
        }
    } else {
        out.push(("Size", size_text(md.size())));
    }
    out.push(("Modified", time_text(md.mtime())));
    out.push(("Accessed", time_text(md.atime())));
    if let Some(created) = md.created().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        out.push(("Created", time_text(created.as_secs() as i64)));
    }
    let mode = md.mode();
    let bits: String = (0..9).map(|i| if mode >> (8 - i) & 1 == 1 { b"rwx"[i % 3] as char } else { '-' }).collect();
    out.push(("Permissions", format!("{bits} ({:03o})", mode & 0o7777)));
    let owner = id_name("/etc/passwd", md.uid());
    let group = id_name("/etc/group", md.gid());
    out.push(("Owner", format!("{owner} : {group}")));
    Ok(out)
}

fn size_text(bytes: u64) -> String {
    let exact = format!("{} bytes", crate::group_digits(bytes));
    let mut value = bytes as f64;
    let mut unit = "";
    for u in ["KB", "MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = u;
    }
    if unit.is_empty() { exact } else { format!("{value:.1} {unit} ({exact})") }
}

fn time_text(secs: i64) -> String {
    let (y, mo, d, h, mi) = crate::utc_civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Looks up a numeric id in /etc/passwd or /etc/group (`name:x:id:...`).
fn id_name(file: &str, id: u32) -> String {
    let text = fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .find_map(|line| {
            let mut f = line.split(':');
            let name = f.next()?;
            (f.nth(1)?.parse::<u32>().ok()? == id).then(|| name.to_string())
        })
        .unwrap_or_else(|| id.to_string())
}

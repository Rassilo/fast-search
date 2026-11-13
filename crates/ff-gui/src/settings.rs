//! Everything behind the Options menu: saved preferences, the launcher
//! entry, the global shortcut and the live-updates service.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// `$VAR`, else `~/fallback`.
fn home_dir(var: &str, fallback: &str) -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(dir.into());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| other("HOME is not set"))?;
    Ok(PathBuf::from(home).join(fallback))
}

/// The program to start for "Fast search": the AppImage file when we run
/// from one (the path inside it only exists while it runs), else this
/// executable.
fn launch_path() -> io::Result<PathBuf> {
    match std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        Some(appimage) => Ok(appimage.into()),
        None => std::env::current_exe(),
    }
}

// ---- saved preferences ---------------------------------------------------

/// Preferences kept in `~/.config/ff/gui.conf` as `key=value` lines.
pub struct Config {
    /// Zoom chosen by the user; `None` follows the desktop scale.
    pub scale: Option<f32>,
    pub own_title_bar: bool,
}

impl Config {
    fn path() -> io::Result<PathBuf> {
        Ok(home_dir("XDG_CONFIG_HOME", ".config")?.join("ff/gui.conf"))
    }

    pub fn load() -> Config {
        let mut config = Config { scale: None, own_title_bar: true };
        let text = Config::path().and_then(fs::read_to_string).unwrap_or_default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("scale", v)) => config.scale = v.trim().parse().ok(),
                Some(("title_bar", v)) => config.own_title_bar = v.trim() != "system",
                _ => {}
            }
        }
        config
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Config::path()?;
        fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let mut text = format!("title_bar={}\n", if self.own_title_bar { "own" } else { "system" });
        if let Some(scale) = self.scale {
            text.push_str(&format!("scale={scale}\n"));
        }
        fs::write(path, text)
    }
}

// ---- applications menu entry ----------------------------------------------

const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
<path d="M5 50A51 51 0 0 1 95 50A51 51 0 0 1 5 50Z" fill="#f08a1c"/>
<path d="M11.34 50A45.5 45.5 0 0 1 88.66 50A45.5 45.5 0 0 1 11.34 50Z" fill="#fdfdfd"/>
<circle cx="50" cy="50" r="17" fill="#c96a12"/>
<circle cx="50" cy="50" r="8.5" fill="#2b2118"/>
<circle cx="55.5" cy="44.5" r="3.5" fill="#fff"/>
</svg>
"##;

/// (desktop entry, icon file, icon theme folder).
fn launcher_paths() -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    let data = home_dir("XDG_DATA_HOME", ".local/share")?;
    let theme = data.join("icons/hicolor");
    Ok((data.join("applications/ff-gui.desktop"), theme.join("scalable/apps/ff-gui.svg"), theme))
}

pub fn launcher_installed() -> bool {
    launcher_paths().is_ok_and(|(entry, ..)| entry.exists())
}

/// A stale icon cache would hide a new icon: rebuild it if we can, and
/// mark the theme folder as changed either way.
fn refresh_icon_cache(theme: &Path) {
    let _ = Command::new("gtk-update-icon-cache").args(["-f", "-t", "-q"]).arg(theme).status();
    let _ = fs::File::open(theme).and_then(|dir| dir.set_modified(SystemTime::now()));
}

/// Writes a desktop entry and icon so the desktop lists "Fast search"
/// with its icon in the applications menu and the dock.
pub fn install_launcher() -> io::Result<PathBuf> {
    let (entry, icon, theme) = launcher_paths()?;
    for file in [&entry, &icon] {
        fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
    }
    fs::write(&icon, ICON_SVG)?;
    refresh_icon_cache(&theme);
    let exe = launch_path()?;
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=Fast search\nComment=Find files instantly by name\n\
         Exec=\"{}\"\nIcon=ff-gui\nTerminal=false\nCategories=Utility;FileTools;\nStartupWMClass=ff-gui\n",
        exe.display()
    );
    fs::write(&entry, text)?;
    Ok(entry)
}

pub fn remove_launcher() -> io::Result<()> {
    let (entry, icon, theme) = launcher_paths()?;
    for file in [entry, icon] {
        match fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    refresh_icon_cache(&theme);
    Ok(())
}

// ---- global shortcut (GNOME) ------------------------------------------------

const KEYS_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const KEY_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/ff/";
const KEY_BINDING: &str = "<Alt>e";

fn gsettings(args: &[&str]) -> io::Result<String> {
    let out = Command::new("gsettings").args(args).stdin(Stdio::null()).output().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => other("this needs GNOME's gsettings command"),
        _ => e,
    })?;
    if !out.status.success() {
        return Err(other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The desktop's list of custom shortcut paths, e.g. `['/a/', '/b/']`.
fn custom_shortcuts() -> io::Result<Vec<String>> {
    let list = gsettings(&["get", KEYS_SCHEMA, "custom-keybindings"])?;
    Ok(list.split('\'').skip(1).step_by(2).map(String::from).collect())
}

pub fn shortcut_enabled() -> bool {
    custom_shortcuts().is_ok_and(|list| list.iter().any(|p| p == KEY_PATH))
}

/// Adds or removes the Alt+E shortcut that starts this program.
/// Returns the new state.
pub fn set_shortcut(on: bool) -> io::Result<bool> {
    let mut list = custom_shortcuts()?;
    list.retain(|p| p != KEY_PATH);
    let ours = format!("{KEYS_SCHEMA}.custom-keybinding:{KEY_PATH}");
    if on {
        let exe = launch_path()?;
        let exe = exe.to_str().ok_or_else(|| other("the program path is not valid UTF-8"))?;
        gsettings(&["set", &ours, "name", "Fast search"])?;
        gsettings(&["set", &ours, "command", exe])?;
        gsettings(&["set", &ours, "binding", KEY_BINDING])?;
        list.push(KEY_PATH.into());
    } else {
        for key in ["name", "command", "binding"] {
            gsettings(&["reset", &ours, key])?;
        }
    }
    let quoted: Vec<String> = list.iter().map(|p| format!("'{p}'")).collect();
    gsettings(&["set", KEYS_SCHEMA, "custom-keybindings", &format!("[{}]", quoted.join(", "))])?;
    Ok(on)
}

// ---- live-updates service (ffd) ---------------------------------------------

const INSTALLED_DAEMON: &str = "/usr/local/bin/ffd";

pub fn daemon_installed() -> bool {
    Path::new("/etc/systemd/system/ffd.service").exists()
}

/// Name of this user's primary group, who will be allowed to read the index.
fn primary_group() -> io::Result<String> {
    let status = fs::read_to_string("/proc/self/status")?;
    let gid = status
        .lines()
        .find_map(|l| l.strip_prefix("Gid:"))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| other("cannot determine your group"))?;
    let groups = fs::read_to_string("/etc/group")?;
    groups
        .lines()
        .find_map(|line| {
            let mut f = line.split(':');
            let name = f.next()?;
            (f.nth(1)? == gid).then(|| name.to_string())
        })
        .ok_or_else(|| other(format!("group {gid} has no name in /etc/group")))
}

/// Installs or removes the root service, asking for the password through
/// the desktop's authorization dialog (pkexec). Blocks until answered;
/// returns a message for the status bar.
pub fn set_daemon(install: bool) -> String {
    let run = || -> io::Result<String> {
        // The ffd binary: next to this program, or already installed.
        let beside = std::env::current_exe()?.with_file_name("ffd");
        let installed = PathBuf::from(INSTALLED_DAEMON);
        // A packaged install keeps ffd in /usr/bin, next to this program.
        let ffd = match (install, beside.exists(), installed.exists()) {
            (true, true, _) | (false, true, false) => beside,
            (_, _, true) => installed.clone(),
            _ => return Err(other(format!("ffd not found next to this program or at {INSTALLED_DAEMON}"))),
        };
        // Inside an AppImage the file is on a mount root cannot read:
        // give the installer a copy it can.
        let ffd = if std::env::var_os("APPIMAGE").is_some() && ffd != installed {
            let copy = std::env::temp_dir().join(format!("ffd-{}", std::process::id()));
            fs::copy(&ffd, &copy)?;
            copy
        } else {
            ffd
        };
        let mut cmd = Command::new("pkexec");
        cmd.arg(ffd);
        if install {
            cmd.args(["--install-service", &primary_group()?]);
        } else {
            cmd.arg("--uninstall-service");
        }
        let out = cmd.stdin(Stdio::null()).output().map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => other("this needs pkexec (polkit) to ask for the password"),
            _ => e,
        })?;
        match out.status.code() {
            Some(0) if install => Ok("Live updates are on: the ffd service is running".into()),
            Some(0) => Ok("Live updates are off: the ffd service was removed".into()),
            Some(126 | 127) => Err(other("not authorized")),
            _ => Err(other(String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed").to_string())),
        }
    };
    run().unwrap_or_else(|e| format!("Live updates not changed: {e}"))
}

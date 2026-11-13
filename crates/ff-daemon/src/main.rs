//! ffd: keeps the ff index current.
//!
//! Runs as root, scans the disk once, then listens to fanotify events for
//! whole filesystems and patches the index as files come and go. The index
//! is published to a file on a RAM-backed filesystem that the unprivileged
//! `ff` and `ff-gui` read; they never talk to this process.

mod sys;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use ff_core::live::LiveIndex;
use ff_core::{BuildOptions, Index};

const USAGE: &str = "\
ffd - keeps the ff index current (run as root)

USAGE:
  ffd [OPTIONS] [ROOT...]             index ROOTs (default /) and follow changes
  ffd --install-service GROUP         install and start ffd as a systemd service
  ffd --uninstall-service             stop and remove that service

OPTIONS:
  -g, --group NAME    group allowed to read the index (default: root only)
  -x, --exclude DIR   skip a directory (repeatable)
  -o, --output FILE   where to publish the index (default /run/ff/index.db)
  -i, --interval SEC  how often to publish after changes (default 2)
  -h, --help

Everyone in GROUP can search the names of ALL files on the system,
including those in folders they cannot open.
";

/// Filesystems worth watching; pseudo and network filesystems are skipped.
const WATCHABLE: &[&str] =
    &["btrfs", "ext2", "ext3", "ext4", "xfs", "f2fs", "bcachefs", "zfs", "vfat", "exfat", "ntfs3", "tmpfs", "overlay"];

/// Where log lines are also written, next to the published index, so the
/// reader group can see what the daemon is doing without root.
static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

fn log(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    eprintln!("ffd: {msg}");
    if let Some(path) = LOG_FILE.get() {
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{msg}");
        }
    }
}

enum Msg {
    /// Something changed about entry `name` in directory `dir`.
    Changed(Vec<u8>, Vec<u8>),
    /// The kernel dropped events; only a full rescan is safe.
    Overflow,
}

struct Config {
    opts: BuildOptions,
    output: PathBuf,
    group: Option<u32>,
    interval: Duration,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("ffd: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn group_id(name: &str) -> Result<u32, String> {
    let text = fs::read_to_string("/etc/group").map_err(|e| format!("/etc/group: {e}"))?;
    text.lines()
        .find_map(|line| {
            let mut f = line.split(':');
            (f.next()? == name).then(|| f.nth(1)?.parse().ok())?
        })
        .ok_or_else(|| format!("no such group: {name}"))
}

fn run() -> Result<(), String> {
    let mut roots = Vec::new();
    let mut excludes = Vec::new();
    let mut cfg = Config {
        opts: BuildOptions::new(["/"]),
        output: ff_core::SYSTEM_DB.into(),
        group: None,
        interval: Duration::from_secs(2),
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.to_str() {
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return Ok(());
            }
            Some("--install-service") => return install_service(&value("--install-service")?.to_string_lossy()),
            Some("--uninstall-service") => return uninstall_service(),
            Some(f @ ("-g" | "--group")) => cfg.group = Some(group_id(&value(f)?.to_string_lossy())?),
            Some(f @ ("-x" | "--exclude")) => excludes.push(PathBuf::from(value(f)?)),
            Some(f @ ("-o" | "--output")) => cfg.output = value(f)?.into(),
            Some(f @ ("-i" | "--interval")) => {
                let secs = value(f)?.to_str().and_then(|s| s.parse::<f64>().ok()).filter(|s| *s >= 0.1);
                cfg.interval = Duration::from_secs_f64(secs.ok_or("--interval needs a number of seconds >= 0.1")?);
            }
            Some(f) if f.starts_with('-') => return Err(format!("unknown option {f} (see ffd --help)")),
            _ => roots.push(PathBuf::from(arg)),
        }
    }
    if !roots.is_empty() {
        cfg.opts = BuildOptions::new(roots);
    }
    cfg.opts.excludes.extend(excludes);
    serve(&cfg)
}

fn serve(cfg: &Config) -> Result<(), String> {
    // Files we create are private to root and the reader group.
    sys::umask(0o027);
    let group = sys::fanotify_init().map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => "fanotify needs root; start ffd with sudo or as a system service".to_string(),
        _ => format!("fanotify_init: {e}"),
    })?;

    prepare_output_dir(cfg)?;
    let log_file = cfg.output.with_file_name("ffd.log");
    let _ = fs::remove_file(&log_file);
    let _ = LOG_FILE.set(log_file);

    // A private mount table lets us mount a filesystem's top level for
    // watching (see `watch_top_level`) without anyone else seeing it.
    let private_mounts = sys::unshare_mounts().is_ok() && command_ok("mount", &["--make-rslave", "/"]);
    if !private_mounts {
        log("could not get a private mount table; btrfs subvolume fallback disabled");
    }

    // Start listening before the scan so nothing that changes during it
    // is missed; those events are applied once the scan is done.
    let mounts = watch_filesystems(&group, cfg, private_mounts)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || read_events(group, mounts, tx));

    let scan = |why: &str| -> Result<LiveIndex, String> {
        let start = Instant::now();
        let index = Index::build(&cfg.opts).map_err(|e| e.to_string())?;
        log(format!("{why}: {} entries in {:.2}s", index.len(), start.elapsed().as_secs_f64()));
        Ok(LiveIndex::new(index))
    };
    let mut live = scan("indexed")?;
    publish(cfg, live.index())?;
    log(format!("publishing to {}", cfg.output.display()));

    let own_dir = cfg.output.parent().map(|p| p.as_os_str().as_bytes().to_vec());
    loop {
        // Sleep until something happens, then gather a batch.
        let first = rx.recv().map_err(|_| "the event reader stopped".to_string())?;
        std::thread::sleep(cfg.interval);
        let mut changes = HashSet::new();
        let mut overflow = false;
        for msg in std::iter::once(first).chain(rx.try_iter()) {
            match msg {
                // Our own index file being rewritten is not news.
                Msg::Changed(dir, _) if Some(&dir) == own_dir.as_ref() => {}
                Msg::Changed(dir, name) => {
                    changes.insert((dir, name));
                }
                Msg::Overflow => overflow = true,
            }
        }
        let changed = if overflow {
            live = scan("events were dropped, rescanned")?;
            true
        } else {
            live.apply(&changes)
        };
        if changed {
            if let Err(e) = publish(cfg, live.index()) {
                log(e);
            }
        }
    }
}

fn command_ok(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program).args(args).status().is_ok_and(|s| s.success())
}

/// Creates the folder the index is published in, readable by the group.
fn prepare_output_dir(cfg: &Config) -> Result<(), String> {
    let fail = |e: io::Error| format!("{}: {e}", cfg.output.display());
    if let Some(dir) = cfg.output.parent().filter(|d| !d.exists()) {
        fs::create_dir_all(dir).map_err(fail)?;
        if let Some(gid) = cfg.group {
            // Set-group-id directory: files created inside inherit the
            // group, so the index is never briefly unreadable.
            std::os::unix::fs::chown(dir, None, Some(gid)).map_err(fail)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o2750)).map_err(fail)?;
        }
    }
    Ok(())
}

/// Writes the index where readers look for it, readable by the group.
fn publish(cfg: &Config, index: &Index) -> Result<(), String> {
    let fail = |e: io::Error| format!("{}: {e}", cfg.output.display());
    prepare_output_dir(cfg)?;
    index.save(&cfg.output).map_err(fail)?;
    if let Some(gid) = cfg.group {
        std::os::unix::fs::chown(&cfg.output, None, Some(gid)).map_err(fail)?;
    }
    Ok(())
}

/// A mounted filesystem that holds indexed files and can be watched.
#[derive(Debug, PartialEq)]
struct Mount {
    point: PathBuf,
    fstype: String,
    /// The device, e.g. `/dev/sda2`.
    source: String,
}

/// Picks from the text of /proc/self/mountinfo the mounts worth watching:
/// real filesystems that hold part of an indexed root and are not excluded.
fn watchable_mounts(mountinfo: &str, roots: &[PathBuf], excludes: &[PathBuf]) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for line in mountinfo.lines() {
        // "id parent major:minor root MOUNTPOINT options... - FSTYPE source ..."
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let mut right = right.split(' ');
        let (Some(point), Some(fstype), Some(source)) = (left.split(' ').nth(4), right.next(), right.next()) else {
            continue;
        };
        let point = PathBuf::from(std::ffi::OsString::from_vec(unescape(point)));
        // Relevant if it holds indexed files: inside a root, or a root is inside it.
        let relevant = roots.iter().any(|r| point.starts_with(r) || r.starts_with(&point));
        let excluded = excludes.iter().any(|x| point.starts_with(x));
        if relevant && !excluded && WATCHABLE.contains(&fstype) {
            mounts.push(Mount { point, fstype: fstype.into(), source: source.into() });
        }
    }
    mounts
}

/// Puts a filesystem-wide watch on every real filesystem under the roots.
/// Returns an open directory per filesystem, used to resolve file handles.
fn watch_filesystems(group: &File, cfg: &Config, private_mounts: bool) -> Result<Vec<File>, String> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").map_err(|e| format!("/proc/self/mountinfo: {e}"))?;
    let roots: Vec<PathBuf> = cfg.opts.roots.iter().filter_map(|r| fs::canonicalize(r).ok()).collect();
    let mask = sys::FAN_CREATE
        | sys::FAN_DELETE
        | sys::FAN_MOVED_FROM
        | sys::FAN_MOVED_TO
        | sys::FAN_CLOSE_WRITE
        | sys::FAN_ONDIR;
    let mut dirs = Vec::new();
    // Per device: did any of its mounts accept a watch?
    let mut devices: Vec<(String, String, bool)> = Vec::new();
    for Mount { point, fstype, source } in watchable_mounts(&mountinfo, &roots, &cfg.opts.excludes) {
        let watched = match sys::fanotify_mark_filesystem(group, mask, &point) {
            Ok(()) => {
                log(format!("watching {} ({fstype})", point.display()));
                true
            }
            Err(e) => {
                log(format!("no direct watch on {} ({fstype}): {e}", point.display()));
                false
            }
        };
        match devices.iter_mut().find(|d| d.0 == source) {
            Some(device) => device.2 |= watched,
            None => devices.push((source, fstype, watched)),
        }
        if let Ok(dir) = File::open(&point) {
            dirs.push(dir);
        }
    }
    for (n, (source, fstype, watched)) in devices.iter().enumerate() {
        if *watched {
            continue;
        }
        if fstype == "btrfs" && private_mounts {
            watch_top_level(group, mask, source, n);
        } else {
            log(format!("{source} ({fstype}) is NOT watched; changes there will be missed"));
        }
    }
    if dirs.is_empty() {
        return Err("found no filesystem to watch".into());
    }
    Ok(dirs)
}

/// btrfs only accepts a filesystem-wide watch on its top-level volume, and
/// systems like openSUSE mount only subvolumes of it. Mount the top level
/// privately (our mount table is our own) and watch that: the watch then
/// covers every subvolume of the device.
fn watch_top_level(group: &File, mask: u64, source: &str, n: usize) {
    let dir = PathBuf::from(format!("/run/ffd-mounts/{n}"));
    let mounted = fs::create_dir_all(&dir).is_ok()
        && command_ok("mount", &["-t", "btrfs", "-o", "subvolid=5,noatime", source, &dir.to_string_lossy()]);
    if !mounted {
        return log(format!("{source}: could not mount the btrfs top level; changes there will be missed"));
    }
    match sys::fanotify_mark_filesystem(group, mask, &dir) {
        Ok(()) => log(format!("watching {source} (btrfs, all subvolumes, via its top level)")),
        Err(e) => log(format!("{source}: watching the btrfs top level failed: {e}; changes there will be missed")),
    }
}

/// Decodes the `\040`-style octal escapes used in /proc/self/mountinfo.
fn unescape(field: &str) -> Vec<u8> {
    let b = field.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let octal = b.get(i + 1..i + 4).filter(|d| b[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        if let Some(d) = octal {
            out.push((d[0] - b'0') << 6 | (d[1] - b'0') << 3 | (d[2] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// One decoded fanotify event.
#[derive(Debug, PartialEq)]
enum Event<'a> {
    /// The kernel dropped events.
    Overflow,
    /// Entry `name` changed in the directory identified by `handle`
    /// (the raw `struct file_handle` bytes).
    Entry { handle: &'a [u8], name: &'a [u8] },
}

/// Decodes the events in one read from a fanotify descriptor. Anything
/// malformed or of a kind we did not ask for is skipped.
fn parse_events(buf: &[u8]) -> Vec<Event<'_>> {
    let u16at = |b: &[u8], i: usize| u16::from_ne_bytes([b[i], b[i + 1]]) as usize;
    let u32at = |b: &[u8], i: usize| u32::from_ne_bytes(b[i..i + 4].try_into().unwrap()) as usize;
    let mut events = Vec::new();
    // Each event: a 24-byte header (length, version, header length,
    // 64-bit mask, fd, pid) followed by information records.
    let mut at = 0;
    while at + 24 <= buf.len() {
        let (event_len, header_len) = (u32at(buf, at), u16at(buf, at + 6));
        if event_len < 24 || at + event_len > buf.len() {
            break;
        }
        let event = &buf[at..at + event_len];
        at += event_len;
        let mask = u64::from_ne_bytes(event[8..16].try_into().unwrap());
        if mask & sys::FAN_Q_OVERFLOW != 0 {
            events.push(Event::Overflow);
            continue;
        }
        // Record: type u8, pad u8, length u16, fsid (8 bytes), then a
        // file handle (size u32, type i32, bytes) and a NUL-ended name.
        let mut rec = header_len;
        while rec + 4 <= event.len() {
            let (kind, len) = (event[rec], u16at(event, rec + 2));
            if len < 4 || rec + len > event.len() {
                break;
            }
            let record = &event[rec..rec + len];
            rec += len;
            const DIR_HANDLE_AND_NAME: u8 = 2;
            if kind != DIR_HANDLE_AND_NAME || record.len() < 20 {
                continue;
            }
            let handle_end = 20 + u32at(record, 12);
            let (Some(handle), Some(name)) = (record.get(12..handle_end), record.get(handle_end..)) else {
                continue;
            };
            let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
            events.push(Event::Entry { handle, name });
        }
    }
    events
}

/// The (directory, name) pair to re-check for an event in directory `dir`.
/// A name of "." means the directory itself, reported under its parent.
fn changed_entry(dir: Vec<u8>, name: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if name != b"." && !name.is_empty() {
        return Some((dir, name.to_vec()));
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(&dir));
    Some((path.parent()?.as_os_str().as_bytes().to_vec(), path.file_name()?.as_bytes().to_vec()))
}

/// Reads fanotify events forever, turning each into a message. Runs on its
/// own thread so directory handles are resolved to paths promptly, before
/// the directory can go away.
fn read_events(mut group: File, mounts: Vec<File>, tx: mpsc::Sender<Msg>) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = match group.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log(format!("reading events failed: {e}"));
                return; // dropping `tx` stops the main loop
            }
        };
        for event in parse_events(&buf[..n]) {
            let msg = match event {
                Event::Overflow => Msg::Overflow,
                Event::Entry { handle, name } => {
                    // No path: the directory is already gone, or it is
                    // not under a mount we index.
                    let Some(dir) = mounts.iter().find_map(|m| handle_path(m, handle)) else { continue };
                    let Some((dir, name)) = changed_entry(dir.into_os_string().into_vec(), name) else { continue };
                    Msg::Changed(dir, name)
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    }
}

/// Path of the directory a file handle refers to, as seen through mount
/// `mount`, or `None` if it cannot be reached that way.
///
/// A handle opens through any mount of its filesystem, but the path is
/// only real through the mount that actually contains the directory
/// (think of btrfs subvolumes mounted at / and /home). So the path is
/// checked: it must lead back to the very same directory.
fn handle_path(mount: &File, handle: &[u8]) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dir = sys::open_by_handle(mount, handle).ok()?;
    let path = fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&dir))).ok()?;
    let (by_handle, by_path) = (dir.metadata().ok()?, fs::metadata(&path).ok()?);
    ((by_handle.dev(), by_handle.ino()) == (by_path.dev(), by_path.ino())).then_some(path)
}

/// `--install-service GROUP`: copies this binary to /usr/local/bin, writes
/// a systemd unit that runs it for GROUP, and starts it.
fn install_service(group: &str) -> Result<(), String> {
    group_id(group)?;
    if !group.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err(format!("unsupported group name: {group}"));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let target = Path::new("/usr/local/bin/ffd");
    let fail = |what: &Path, e: io::Error| format!("{}: {e} (run this with sudo)", what.display());
    if exe != target {
        // Copy to a temporary name and rename, so a running ffd is not
        // overwritten in place.
        let tmp = target.with_extension("new");
        fs::copy(&exe, &tmp).map_err(|e| fail(&tmp, e))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).map_err(|e| fail(&tmp, e))?;
        fs::rename(&tmp, target).map_err(|e| fail(target, e))?;
    }
    let unit = Path::new("/etc/systemd/system/ffd.service");
    let text = format!(
        "[Unit]\nDescription=Fast search index daemon (ffd)\n\n\
         [Service]\nExecStart={} --group {group}\nRestart=on-failure\nNice=10\nNoNewPrivileges=yes\n\n\
         [Install]\nWantedBy=multi-user.target\n",
        target.display()
    );
    fs::write(unit, text).map_err(|e| fail(unit, e))?;
    println!("installed {} and {}", target.display(), unit.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "ffd"])?;
    systemctl(&["restart", "ffd"])?;
    println!("the ffd service is running");
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    match std::process::Command::new("systemctl").args(args).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("systemctl {} failed ({s})", args.join(" "))),
        Err(e) => Err(format!("cannot run systemctl: {e}")),
    }
}

/// `--uninstall-service`: stops the service and removes what
/// `--install-service` created.
fn uninstall_service() -> Result<(), String> {
    let _ = systemctl(&["disable", "--now", "ffd"]); // may not be installed
    for file in ["/etc/systemd/system/ffd.service", "/usr/local/bin/ffd"] {
        match fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(format!("{file}: {e} (run this with sudo)")),
            _ => {}
        }
    }
    let _ = fs::remove_dir_all("/run/ff");
    systemctl(&["daemon-reload"])?;
    println!("the ffd service is removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one fanotify event as the kernel lays it out.
    fn event(mask: u64, records: &[(u8, &[u8], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for &(kind, handle_bytes, name) in records {
            let mut rec = vec![kind, 0, 0, 0];
            rec.extend([0xAA; 8]); // fsid
            rec.extend((handle_bytes.len() as u32).to_ne_bytes());
            rec.extend(1i32.to_ne_bytes()); // handle type
            rec.extend(handle_bytes);
            rec.extend(name);
            rec.push(0);
            rec.resize(rec.len().next_multiple_of(4), 0); // the kernel pads records
            let len = rec.len() as u16;
            rec[2..4].copy_from_slice(&len.to_ne_bytes());
            body.extend(rec);
        }
        let mut ev = Vec::new();
        ev.extend((24 + body.len() as u32).to_ne_bytes());
        ev.extend([3, 0]); // version, reserved
        ev.extend(24u16.to_ne_bytes());
        ev.extend(mask.to_ne_bytes());
        ev.extend((-1i32).to_ne_bytes()); // no fd in this mode
        ev.extend(0i32.to_ne_bytes()); // pid
        ev.extend(body);
        ev
    }

    /// The handle slice the parser should hand to open_by_handle_at.
    fn handle(bytes: &[u8]) -> Vec<u8> {
        let mut h = (bytes.len() as u32).to_ne_bytes().to_vec();
        h.extend(1i32.to_ne_bytes());
        h.extend(bytes);
        h
    }

    #[test]
    fn decodes_events_with_names() {
        let mut buf = event(sys::FAN_CREATE, &[(2, b"HANDLE-1", b"new.txt")]);
        buf.extend(event(sys::FAN_DELETE | sys::FAN_ONDIR, &[(2, b"H2", b"old dir")]));
        let (h1, h2) = (handle(b"HANDLE-1"), handle(b"H2"));
        assert_eq!(
            parse_events(&buf),
            [Event::Entry { handle: &h1, name: b"new.txt" }, Event::Entry { handle: &h2, name: b"old dir" }]
        );
    }

    #[test]
    fn decodes_overflow_and_skips_other_records() {
        let mut buf = event(sys::FAN_Q_OVERFLOW, &[]);
        // Record kinds 1 (file id) and 3 (directory id, no name) are not ours.
        buf.extend(event(sys::FAN_CREATE, &[(1, b"FID", b""), (3, b"DFID", b""), (2, b"H", b"x")]));
        let h = handle(b"H");
        assert_eq!(parse_events(&buf), [Event::Overflow, Event::Entry { handle: &h, name: b"x" }]);
    }

    #[test]
    fn survives_truncated_and_garbage_input() {
        let good = event(sys::FAN_CREATE, &[(2, b"HANDLE", b"file")]);
        for cut in 0..good.len() {
            assert!(parse_events(&good[..cut]).is_empty(), "cut at {cut}");
        }
        assert!(parse_events(&[0xFF; 64]).is_empty());
        assert!(parse_events(&[0; 64]).is_empty());
        // A record claiming a handle longer than the record is skipped.
        let mut bad = event(sys::FAN_CREATE, &[(2, b"HANDLE", b"file")]);
        bad[24 + 12..24 + 16].copy_from_slice(&1000u32.to_ne_bytes());
        assert!(parse_events(&bad).is_empty());
    }

    #[test]
    fn dot_means_the_directory_itself() {
        let pair = |d: &str, n: &str| Some((d.as_bytes().to_vec(), n.as_bytes().to_vec()));
        assert_eq!(changed_entry(b"/home/me".to_vec(), b"notes.txt"), pair("/home/me", "notes.txt"));
        assert_eq!(changed_entry(b"/home/me/docs".to_vec(), b"."), pair("/home/me", "docs"));
        assert_eq!(changed_entry(b"/home/me/docs".to_vec(), b""), pair("/home/me", "docs"));
        assert_eq!(changed_entry(b"/".to_vec(), b"."), None); // the root has no parent
    }

    #[test]
    fn unescapes_mount_points() {
        assert_eq!(unescape("/mnt/my\\040disk"), b"/mnt/my disk");
        assert_eq!(unescape("/a\\011b\\134c"), b"/a\tb\\c");
        assert_eq!(unescape("/plain"), b"/plain");
        assert_eq!(unescape("/trailing\\04"), b"/trailing\\04"); // not a full escape
    }

    // A mount table like openSUSE's: one btrfs device, every folder a
    // subvolume, plus the usual pseudo filesystems.
    const MOUNTINFO: &str = "\
23 46 0:22 / /proc rw,nosuid shared:13 - proc proc rw
24 46 0:23 / /sys rw,nosuid shared:2 - sysfs sysfs rw,seclabel
25 46 0:6 / /dev rw,nosuid shared:9 - devtmpfs devtmpfs rw,seclabel,size=4096k
27 46 0:31 / /run rw,nosuid,nodev shared:11 - tmpfs tmpfs rw,seclabel,mode=755
46 1 0:42 /@/.snapshots/1/snapshot / rw,relatime shared:1 - btrfs /dev/nvme1n1p3 rw,subvolid=264
44 46 0:51 /@/home /home rw,relatime shared:55 - btrfs /dev/nvme1n1p3 rw,subvolid=262
45 46 0:50 /@/opt /opt rw,relatime shared:57 - btrfs /dev/nvme1n1p3 rw,subvolid=261
47 46 0:41 /@/.snapshots /.snapshots rw,relatime shared:59 - btrfs /dev/nvme1n1p3 rw,subvolid=265
48 46 0:47 / /tmp rw,nosuid,nodev shared:61 - tmpfs tmpfs rw,seclabel
49 46 259:1 / /boot/efi rw,relatime shared:63 - vfat /dev/nvme1n1p1 rw,fmask=0022
50 27 0:78 / /run/user/1000 rw,nosuid,nodev shared:65 - tmpfs tmpfs rw,seclabel
51 50 0:80 / /run/user/1000/gvfs rw,nosuid,nodev shared:67 - fuse.gvfsd-fuse gvfsd-fuse rw
52 46 8:17 / /mnt/my\\040disk rw,relatime shared:69 - ext4 /dev/sdb1 rw
53 46 0:90 / /mnt/nas rw,relatime shared:71 - nfs4 nas:/export rw
malformed line without the separator
";

    fn points(mounts: &[Mount]) -> Vec<&str> {
        mounts.iter().map(|m| m.point.to_str().unwrap()).collect()
    }

    #[test]
    fn picks_real_filesystems_under_the_roots() {
        let excludes = ["/proc", "/sys", "/dev", "/run", "/.snapshots"].map(PathBuf::from);
        let all = watchable_mounts(MOUNTINFO, &["/".into()], &excludes);
        // Pseudo, network and FUSE filesystems and excluded folders are out.
        assert_eq!(points(&all), ["/", "/home", "/opt", "/tmp", "/boot/efi", "/mnt/my disk"]);
        assert_eq!(all[1], Mount { point: "/home".into(), fstype: "btrfs".into(), source: "/dev/nvme1n1p3".into() });
        assert_eq!(all[5].source, "/dev/sdb1");

        // Indexing only a folder: its own mount, mounts below it, and the
        // mount that contains it.
        let home = watchable_mounts(MOUNTINFO, &["/home/me/work".into()], &excludes);
        assert_eq!(points(&home), ["/", "/home"]);
        let mnt = watchable_mounts(MOUNTINFO, &["/mnt".into()], &excludes);
        assert_eq!(points(&mnt), ["/", "/mnt/my disk"]);

        // Without the default excludes, /run's tmpfs mounts are included.
        let none = watchable_mounts(MOUNTINFO, &["/run".into()], &[]);
        assert_eq!(points(&none), ["/run", "/", "/run/user/1000"]);
    }
}

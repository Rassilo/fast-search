//! The few Linux system calls the daemon needs that the standard library
//! does not expose, made directly so the binary needs no C library crate.

use std::arch::asm;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("ffd currently supports x86-64 Linux only");

const SYS_UMASK: usize = 95;
const SYS_FANOTIFY_INIT: usize = 300;
const SYS_FANOTIFY_MARK: usize = 301;
const SYS_OPEN_BY_HANDLE_AT: usize = 304;

const O_CLOEXEC: usize = 0x80000;
const O_PATH: usize = 0x200000;
const AT_FDCWD: usize = -100isize as usize;

const FAN_CLOEXEC: usize = 0x1;
const FAN_UNLIMITED_QUEUE: usize = 0x10;
const FAN_UNLIMITED_MARKS: usize = 0x20;
/// Report the parent directory's handle and the entry name with events.
const FAN_REPORT_DFID_NAME: usize = 0xc00;
const FAN_MARK_ADD: usize = 0x1;
const FAN_MARK_FILESYSTEM: usize = 0x100;

pub const FAN_CLOSE_WRITE: u64 = 0x8;
pub const FAN_MOVED_FROM: u64 = 0x40;
pub const FAN_MOVED_TO: u64 = 0x80;
pub const FAN_CREATE: u64 = 0x100;
pub const FAN_DELETE: u64 = 0x200;
pub const FAN_Q_OVERFLOW: u64 = 0x4000;
pub const FAN_ONDIR: u64 = 0x4000_0000;

fn syscall(number: usize, args: [usize; 5]) -> io::Result<usize> {
    let ret: isize;
    // SAFETY: the x86-64 Linux syscall convention; `syscall` clobbers only
    // rcx and r11. Each caller passes arguments valid for its call.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => ret,
            in("rdi") args[0],
            in("rsi") args[1],
            in("rdx") args[2],
            in("r10") args[3],
            in("r8") args[4],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if (-4095..0).contains(&ret) { Err(io::Error::from_raw_os_error(-ret as i32)) } else { Ok(ret as usize) }
}

/// Gives this process its own copy of the mount table, so mounts it makes
/// are invisible to the rest of the system.
pub fn unshare_mounts() -> io::Result<()> {
    const SYS_UNSHARE: usize = 272;
    const CLONE_NEWNS: usize = 0x20000;
    syscall(SYS_UNSHARE, [CLONE_NEWNS, 0, 0, 0, 0]).map(|_| ())
}

/// Sets the permission mask for files this process creates.
pub fn umask(mask: u32) {
    let _ = syscall(SYS_UMASK, [mask as usize, 0, 0, 0, 0]);
}

/// Opens a fanotify group whose events name the directory and entry that
/// changed. Needs root (CAP_SYS_ADMIN).
pub fn fanotify_init() -> io::Result<File> {
    let flags = FAN_CLOEXEC | FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS | FAN_REPORT_DFID_NAME;
    let fd = syscall(SYS_FANOTIFY_INIT, [flags, O_CLOEXEC, 0, 0, 0])?;
    // SAFETY: the kernel just gave us this descriptor; nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

/// Watches the whole filesystem that `mount_point` is on for `mask`.
pub fn fanotify_mark_filesystem(group: &File, mask: u64, mount_point: &Path) -> io::Result<()> {
    let path = CString::new(mount_point.as_os_str().as_bytes())?;
    let args = [
        group.as_raw_fd() as usize,
        FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
        mask as usize,
        AT_FDCWD,
        path.as_ptr() as usize,
    ];
    syscall(SYS_FANOTIFY_MARK, args).map(|_| ())
}

/// Opens the object a kernel file handle refers to (as a path-only
/// descriptor). `mount` is any open file on the same filesystem and
/// `handle` the raw `struct file_handle` bytes from an event.
pub fn open_by_handle(mount: &File, handle: &[u8]) -> io::Result<File> {
    let args = [mount.as_raw_fd() as usize, handle.as_ptr() as usize, O_PATH | O_CLOEXEC, 0, 0];
    let fd = syscall(SYS_OPEN_BY_HANDLE_AT, args)?;
    // SAFETY: as in `fanotify_init`.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

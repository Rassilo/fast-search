use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::thread;

use crate::index::{FLAG_DIR, FLAG_SYMLINK, Index, NO_PARENT};

/// What to index.
pub struct BuildOptions {
    /// Directories to index recursively.
    pub roots: Vec<PathBuf>,
    /// Directories to skip entirely (absolute paths).
    pub excludes: Vec<PathBuf>,
    /// Walker threads; 0 picks a default from the CPU count.
    pub threads: usize,
}

impl BuildOptions {
    /// Indexes `roots`, skipping the kernel pseudo-filesystems and btrfs
    /// snapshot directory by default.
    pub fn new<P: Into<PathBuf>>(roots: impl IntoIterator<Item = P>) -> Self {
        BuildOptions {
            roots: roots.into_iter().map(Into::into).collect(),
            excludes: ["/proc", "/sys", "/dev", "/run", "/.snapshots"].map(PathBuf::from).into(),
            threads: 0,
        }
    }

    /// Options that rebuild `old` with the same roots and excludes
    /// (root `/` with the default excludes if `old` is empty).
    pub fn refresh(old: &Index) -> Self {
        if old.is_empty() {
            return Self::new(["/"]);
        }
        let mut opts = Self::new(old.roots().map(|r| old.path(r)));
        opts.excludes = old.excludes().collect();
        opts
    }
}

struct Queue {
    /// Directories waiting to be read, with their entry id.
    stack: Vec<(PathBuf, u32)>,
    /// Workers currently reading a directory (they may still add work).
    active: usize,
}

impl Index {
    /// Walks the roots in parallel and builds a fresh index. Symlinks are
    /// recorded but not followed; unreadable directories are skipped.
    pub fn build(opts: &BuildOptions) -> io::Result<Index> {
        let mut roots = Vec::new();
        for r in &opts.roots {
            let p = fs::canonicalize(r).map_err(|e| with_path(e, r))?;
            if !p.is_dir() {
                return Err(with_path(io::ErrorKind::NotADirectory.into(), &p));
            }
            roots.push(p);
        }
        // Drop duplicates and roots nested in another root (ancestors sort first).
        roots.sort();
        let mut kept: Vec<PathBuf> = Vec::new();
        for r in roots {
            if !kept.iter().any(|k| r.starts_with(k)) {
                kept.push(r);
            }
        }

        let mut index = Index::new();
        index.excludes = opts
            .excludes
            .iter()
            .map(|e| fs::canonicalize(e).unwrap_or_else(|_| e.clone()).into_os_string().into_encoded_bytes())
            .collect();

        let mut stack = Vec::new();
        for r in kept {
            let md = fs::metadata(&r).map_err(|e| with_path(e, &r))?;
            let id = index.push(r.as_os_str().as_bytes(), NO_PARENT, FLAG_DIR, md.size(), md.mtime());
            stack.push((r, id));
        }

        let threads = match opts.threads {
            0 => thread::available_parallelism().map_or(4, |n| n.get()).min(16),
            n => n,
        };
        let excludes = std::mem::take(&mut index.excludes);
        let index = Mutex::new(index);
        let queue = (Mutex::new(Queue { stack, active: 0 }), Condvar::new());
        thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| worker(&queue, &index, &excludes));
            }
        });
        let mut index = index.into_inner().unwrap();
        index.excludes = excludes;
        Ok(index)
    }
}

fn with_path(e: io::Error, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

fn worker(queue: &(Mutex<Queue>, Condvar), index: &Mutex<Index>, excludes: &[Vec<u8>]) {
    let (lock, cond) = queue;
    let mut batch: Vec<(OsString, u8, u64, i64)> = Vec::new();
    let mut dirs: Vec<(OsString, u32)> = Vec::new();
    loop {
        let (dir, dir_id) = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(job) = q.stack.pop() {
                    q.active += 1;
                    break job;
                }
                if q.active == 0 {
                    cond.notify_all();
                    return;
                }
                q = cond.wait(q).unwrap();
            }
        };

        if let Ok(rd) = fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let (flags, size, mtime) = match entry.metadata() {
                    Ok(md) => (type_flags(md.file_type()), md.size(), md.mtime()),
                    Err(_) => (entry.file_type().map_or(0, type_flags), 0, 0),
                };
                batch.push((entry.file_name(), flags, size, mtime));
            }
        }

        // One short critical section per directory keeps contention low.
        {
            let mut ix = index.lock().unwrap();
            for (name, flags, size, mtime) in batch.drain(..) {
                let id = ix.push(name.as_bytes(), dir_id, flags, size, mtime);
                if flags == FLAG_DIR {
                    dirs.push((name, id));
                }
            }
        }

        let subdirs: Vec<(PathBuf, u32)> = dirs
            .drain(..)
            .map(|(name, id)| (dir.join(name), id))
            .filter(|(p, _)| !excludes.iter().any(|x| x == p.as_os_str().as_bytes()))
            .collect();

        let mut q = lock.lock().unwrap();
        q.stack.extend(subdirs);
        q.active -= 1;
        drop(q);
        cond.notify_all();
    }
}

pub(crate) fn type_flags(ft: fs::FileType) -> u8 {
    if ft.is_symlink() {
        FLAG_SYMLINK
    } else if ft.is_dir() {
        FLAG_DIR
    } else {
        0
    }
}

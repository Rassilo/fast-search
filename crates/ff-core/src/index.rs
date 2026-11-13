use std::ffi::OsString;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// Parent id of a root entry.
pub const NO_PARENT: u32 = u32::MAX;
pub const FLAG_DIR: u8 = 1;
pub const FLAG_SYMLINK: u8 = 2;

const MAGIC: &[u8; 8] = b"FFIDX\x00\x00\x01";

/// The file database. Entries are identified by a dense `u32` id; a parent
/// always has a smaller id than its children.
pub struct Index {
    /// All names back to back; entry `i` is `names[offs[i]..offs[i + 1]]`.
    pub(crate) names: Vec<u8>,
    pub(crate) offs: Vec<u32>,
    pub(crate) parent: Vec<u32>,
    pub(crate) size: Vec<u64>,
    pub(crate) mtime: Vec<i64>,
    pub(crate) flags: Vec<u8>,
    /// Directories that were skipped when building (absolute paths).
    pub(crate) excludes: Vec<Vec<u8>>,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    pub fn new() -> Self {
        Index {
            names: Vec::new(),
            offs: vec![0],
            parent: Vec::new(),
            size: Vec::new(),
            mtime: Vec::new(),
            flags: Vec::new(),
            excludes: Vec::new(),
        }
    }

    /// Number of entries (files and directories).
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// File name of an entry (for a root: its full path).
    pub fn name(&self, id: u32) -> &[u8] {
        let i = id as usize;
        &self.names[self.offs[i] as usize..self.offs[i + 1] as usize]
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        let p = self.parent[id as usize];
        (p != NO_PARENT).then_some(p)
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.flags[id as usize] & FLAG_DIR != 0
    }

    pub fn is_symlink(&self, id: u32) -> bool {
        self.flags[id as usize] & FLAG_SYMLINK != 0
    }

    /// Size in bytes.
    pub fn size(&self, id: u32) -> u64 {
        self.size[id as usize]
    }

    /// Modification time, seconds since the Unix epoch.
    pub fn mtime(&self, id: u32) -> i64 {
        self.mtime[id as usize]
    }

    /// Ids of the indexed roots.
    pub fn roots(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len() as u32).filter(|&i| self.parent[i as usize] == NO_PARENT)
    }

    /// Directories skipped at build time.
    pub fn excludes(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.excludes.iter().map(|e| OsString::from_vec(e.clone()).into())
    }

    /// Bytes of heap used by the index.
    pub fn memory_usage(&self) -> usize {
        self.names.len() + self.offs.len() * 4 + self.len() * (4 + 8 + 8 + 1)
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut buf = Vec::new();
        self.write_path(id, &mut buf);
        OsString::from_vec(buf).into()
    }

    /// Writes the full path of `id` into `buf` (replacing its contents).
    /// Reuse the buffer across calls to avoid allocations.
    pub fn write_path(&self, id: u32, buf: &mut Vec<u8>) {
        // Pass 1: total length. A separator goes before every non-root
        // component unless the parent already ends in '/' (the "/" root).
        let mut len = 0;
        let mut cur = id;
        loop {
            len += self.name(cur).len();
            let p = self.parent[cur as usize];
            if p == NO_PARENT {
                break;
            }
            len += !self.name(p).ends_with(b"/") as usize;
            cur = p;
        }
        // Pass 2: fill from the back.
        buf.clear();
        buf.resize(len, 0);
        let mut end = len;
        let mut cur = id;
        loop {
            let name = self.name(cur);
            buf[end - name.len()..end].copy_from_slice(name);
            end -= name.len();
            let p = self.parent[cur as usize];
            if p == NO_PARENT {
                break;
            }
            if !self.name(p).ends_with(b"/") {
                end -= 1;
                buf[end] = b'/';
            }
            cur = p;
        }
    }

    pub(crate) fn push(&mut self, name: &[u8], parent: u32, flags: u8, size: u64, mtime: i64) -> u32 {
        let id = self.len();
        let end = self.names.len() + name.len();
        assert!(id < NO_PARENT as usize && end <= u32::MAX as usize, "index full");
        self.names.extend_from_slice(name);
        self.offs.push(end as u32);
        self.parent.push(parent);
        self.size.push(size);
        self.mtime.push(mtime);
        self.flags.push(flags);
        id as u32
    }

    /// Drops every entry whose `gone` flag is set, renumbering the rest.
    /// A removed entry's children must be flagged too.
    pub(crate) fn compact(&mut self, gone: &[bool]) {
        let mut kept = Index::new();
        kept.excludes = std::mem::take(&mut self.excludes);
        let mut new_id = vec![NO_PARENT; self.len()];
        for id in 0..self.len() {
            if gone[id] {
                continue;
            }
            let parent = match self.parent[id] {
                NO_PARENT => NO_PARENT,
                p => new_id[p as usize],
            };
            new_id[id] = kept.push(self.name(id as u32), parent, self.flags[id], self.size[id], self.mtime[id]);
        }
        *self = kept;
    }

    /// Writes the index to `path` atomically (temp file + rename).
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp{}", std::process::id()));
        let res = self.write_to(Path::new(&tmp)).and_then(|()| fs::rename(&tmp, path));
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res
    }

    fn write_to(&self, path: &Path) -> io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, fs::File::create(path)?);
        w.write_all(MAGIC)?;
        for v in [self.len(), self.names.len(), self.excludes.len()] {
            w.write_all(&(v as u32).to_le_bytes())?;
        }
        for v in &self.offs {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in &self.parent {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in &self.size {
            w.write_all(&v.to_le_bytes())?;
        }
        for v in &self.mtime {
            w.write_all(&v.to_le_bytes())?;
        }
        w.write_all(&self.flags)?;
        w.write_all(&self.names)?;
        for e in &self.excludes {
            w.write_all(&(e.len() as u32).to_le_bytes())?;
            w.write_all(e)?;
        }
        w.flush()
    }

    /// Loads an index written by [`Index::save`]. The file is validated, so a
    /// truncated or corrupt file yields an error rather than a bad index.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Index> {
        let data = fs::read(path)?;
        Self::parse(&data).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "not a valid ff index (rebuild it)")
        })
    }

    fn parse(data: &[u8]) -> Option<Index> {
        let mut r = Reader(data);
        if r.take(8)? != MAGIC {
            return None;
        }
        let n = r.u32()? as usize;
        let names_len = r.u32()? as usize;
        let n_excl = r.u32()? as usize;

        let offs: Vec<u32> = le(r.take((n + 1) * 4)?, u32::from_le_bytes);
        let parent: Vec<u32> = le(r.take(n * 4)?, u32::from_le_bytes);
        let size: Vec<u64> = le(r.take(n * 8)?, u64::from_le_bytes);
        let mtime: Vec<i64> = le(r.take(n * 8)?, i64::from_le_bytes);
        let flags = r.take(n)?.to_vec();
        let names = r.take(names_len)?.to_vec();
        let mut excludes = Vec::new();
        for _ in 0..n_excl {
            let len = r.u32()? as usize;
            excludes.push(r.take(len)?.to_vec());
        }

        let offs_ok = offs[0] == 0
            && offs[n] as usize == names_len
            && offs.windows(2).all(|w| w[0] <= w[1]);
        let parents_ok = parent
            .iter()
            .enumerate()
            .all(|(i, &p)| p == NO_PARENT || (p as usize) < i);
        (offs_ok && parents_ok).then_some(Index { names, offs, parent, size, mtime, flags, excludes })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

fn le<T, const N: usize>(bytes: &[u8], f: fn([u8; N]) -> T) -> Vec<T> {
    bytes.chunks_exact(N).map(|c| f(c.try_into().unwrap())).collect()
}

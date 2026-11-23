//! Keeping an index in step with the filesystem.
//!
//! A watcher ([`crate::watch`]) reports *where* something changed as
//! (directory, name) pairs; [`Tracker::apply`] then looks at the disk to
//! learn *what* is true now and patches the index. Not interpreting event
//! types keeps this correct whatever the order or kind of events (create,
//! delete, rename, write).

use std::collections::{HashMap, HashSet};
use std::fs;

use crate::index::{FLAG_DIR, Index, NO_PARENT};
use crate::walk::type_flags;

/// What it takes to patch an index quickly: its directories by path. Paths
/// are kept as 64-bit hashes (a few bytes per directory instead of the
/// path), and a lookup checks the real path, so a collision only means a
/// change is not seen.
pub struct Tracker {
    /// Hash of the full path of every indexed directory -> its entry id.
    dirs: HashMap<u64, u32>,
    /// Names of roots that never change (drives read directly while not
    /// mounted): their directories are left out of `dirs`.
    fixed_roots: Vec<Vec<u8>>,
    /// Removed entries are marked gone in place, keeping every id valid
    /// (for a window showing them), instead of being dropped.
    tombstones: bool,
}

const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, continued from `h`: hashing a path in pieces gives the same as
/// hashing it whole.
fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn ends_with_sep(path: &[u8]) -> bool {
    path.last().is_some_and(|&b| crate::os::is_sep(b))
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !ends_with_sep(&path) {
        path.push(crate::os::SEP);
    }
    path.extend_from_slice(name);
    path
}

impl Tracker {
    /// For an index that stays in use while it is patched: removed entries
    /// are marked gone ([`Index::remove`]) and ids never change.
    pub fn new(index: &Index) -> Tracker {
        Self::build(index, Vec::new(), true)
    }

    fn build(index: &Index, fixed_roots: Vec<Vec<u8>>, tombstones: bool) -> Tracker {
        let mut t = Tracker { dirs: HashMap::new(), fixed_roots, tombstones };
        t.rebuild(index);
        t
    }

    /// Hashes every directory's path, each from its parent's: one pass,
    /// since parents come before their children.
    fn rebuild(&mut self, index: &Index) {
        self.dirs.clear();
        let mut hashes: HashMap<u32, u64> = HashMap::new();
        for id in 0..index.len() {
            // Gone directories have another flag value, and are left out.
            if index.flags[id] != FLAG_DIR {
                continue;
            }
            let parent = index.parent[id];
            let name = index.name(id as u32);
            let h = if parent == NO_PARENT {
                if self.fixed_roots.iter().any(|root| root == name) {
                    continue;
                }
                fnv(FNV_START, name)
            } else {
                // A parent missing here is fixed or gone, and so is this.
                let Some(&h) = hashes.get(&parent) else { continue };
                let h = if ends_with_sep(index.name(parent)) { h } else { fnv(h, &[crate::os::SEP]) };
                fnv(h, name)
            };
            hashes.insert(id as u32, h);
            self.dirs.insert(h, id as u32);
        }
        self.dirs.shrink_to_fit();
    }

    /// The indexed directory at `path`, if any.
    fn dir_id(&self, index: &Index, path: &[u8], buf: &mut Vec<u8>) -> Option<u32> {
        let &id = self.dirs.get(&fnv(FNV_START, path))?;
        if index.is_gone(id) {
            return None;
        }
        index.write_path(id, buf);
        (buf[..] == *path).then_some(id)
    }

    /// True if `path` is an excluded directory or inside one.
    fn excluded(index: &Index, path: &[u8]) -> bool {
        // With hidden names left out, so is anything below a hidden folder.
        let hidden = index.skips_hidden() && path.split(|&b| crate::os::is_sep(b)).any(|part| part.starts_with(b"."));
        hidden
            || crate::walk::in_trash(path)
            || index.excludes.iter().any(|x| path.strip_prefix(&x[..]).is_some_and(|rest| rest.is_empty() || crate::os::is_sep(rest[0]) || ends_with_sep(x)))
    }

    /// Re-reads the given (directory path, entry name) pairs from disk and
    /// updates `index` to match. Returns true if the index changed.
    pub fn apply(&mut self, index: &mut Index, changes: &HashSet<(Vec<u8>, Vec<u8>)>) -> bool {
        // Which names to look for under which indexed directory.
        let mut wanted: HashMap<u32, HashSet<&[u8]>> = HashMap::new();
        let skip_hidden = index.skips_hidden();
        let mut buf = Vec::new();
        for (dir, name) in changes {
            if skip_hidden && name.starts_with(b".") {
                continue;
            }
            if let Some(id) = self.dir_id(index, dir, &mut buf).filter(|_| !Self::excluded(index, dir)) {
                wanted.entry(id).or_default().insert(name);
            }
        }
        if wanted.is_empty() {
            return false;
        }
        // One pass over the index finds the entries that already exist. A
        // few directories are compared directly, faster than hashing each
        // of millions of parents.
        let few: Vec<u32> = if wanted.len() <= 16 { wanted.keys().copied().collect() } else { Vec::new() };
        let mut existing: HashMap<(u32, &[u8]), u32> = HashMap::new();
        for id in 0..index.len() as u32 {
            let parent = index.parent[id as usize];
            if !few.is_empty() && !few.contains(&parent) {
                continue;
            }
            if wanted.get(&parent).is_some_and(|names| names.contains(index.name(id))) && !index.is_gone(id) {
                existing.insert((parent, index.name(id)), id);
            }
        }
        // Owned copies, so the index can be modified while we work.
        let work: Vec<(u32, Vec<u8>, Option<u32>)> = wanted
            .iter()
            .flat_map(|(&dir, names)| names.iter().map(move |&n| (dir, n)))
            .map(|(dir, n)| (dir, n.to_vec(), existing.get(&(dir, n)).copied()))
            .collect();

        let mut changed = false;
        let mut dead: Vec<u32> = Vec::new();
        let mut dir_path = Vec::new();
        for (dir, name, old) in work {
            index.write_path(dir, &mut dir_path);
            let path = join(&dir_path, &name);
            let md = fs::symlink_metadata(crate::os::path_from_bytes(&path)).ok();
            let flags = md.as_ref().map(|m| type_flags(m.file_type()));
            match (md, old) {
                // Still there, same kind: refresh size and time in place.
                (Some(md), Some(id)) if flags == Some(index.flags[id as usize]) => {
                    let i = id as usize;
                    changed |= (index.size[i], index.mtime[i]) != (crate::os::size(&md), crate::os::mtime(&md));
                    index.set_meta(id, crate::os::size(&md), crate::os::mtime(&md));
                }
                (md, old) => {
                    changed |= md.is_some() || old.is_some();
                    dead.extend(old); // gone, or replaced by another kind
                    if let Some(md) = md {
                        self.add_tree(index, path, dir, &name, &md);
                    }
                }
            }
        }
        if !dead.is_empty() {
            if self.tombstones {
                // Their hashes stay behind, pointing at gone entries, which
                // lookups skip; a folder made again at the same path takes
                // its hash over.
                index.remove(&dead);
            } else {
                // Children always follow their parent, so one ascending
                // pass carries a removal down to everything below it.
                let mut gone = vec![false; index.len()];
                for id in dead {
                    gone[id as usize] = true;
                }
                for i in 0..gone.len() {
                    let parent = index.parent[i] as usize;
                    gone[i] = gone[i] || gone.get(parent).copied().unwrap_or(false);
                }
                index.compact(&gone);
                self.rebuild(index);
            }
        }
        changed
    }

    /// Adds an entry and, for a directory, everything inside it.
    fn add_tree(&mut self, index: &mut Index, path: Vec<u8>, parent: u32, name: &[u8], md: &fs::Metadata) {
        let flags = type_flags(md.file_type());
        let id = index.push(name, parent, flags, crate::os::size(md), crate::os::mtime(md));
        if flags != FLAG_DIR {
            return;
        }
        let mut stack = vec![(path, id)];
        while let Some((dir, dir_id)) = stack.pop() {
            self.dirs.insert(fnv(FNV_START, &dir), dir_id);
            if Self::excluded(index, &dir) {
                continue; // listed, but never descended into
            }
            let Ok(rd) = fs::read_dir(crate::os::path_from_bytes(&dir)) else { continue };
            for entry in rd.flatten() {
                let Ok(md) = entry.metadata() else { continue };
                let name = entry.file_name();
                if index.skips_hidden() && (crate::os::bytes(&name).starts_with(b".") || crate::os::hidden_attribute(&md)) {
                    continue;
                }
                let flags = type_flags(md.file_type());
                let id = index.push(crate::os::bytes(&name), dir_id, flags, crate::os::size(&md), crate::os::mtime(&md));
                if flags == FLAG_DIR {
                    stack.push((join(&dir, crate::os::bytes(&name)), id));
                }
            }
        }
    }
}

/// An index plus its [`Tracker`], for a process that owns the index alone
/// (the daemon): removed entries are dropped and ids renumbered.
pub struct LiveIndex {
    index: Index,
    tracker: Tracker,
}

impl LiveIndex {
    pub fn new(index: Index) -> LiveIndex {
        Self::with_fixed_roots(index, Vec::new())
    }

    /// As `new`, for an index where the roots with these names cannot
    /// change, so nothing needs to be remembered about them.
    pub fn with_fixed_roots(index: Index, fixed_roots: Vec<Vec<u8>>) -> LiveIndex {
        let tracker = Tracker::build(&index, fixed_roots, false);
        LiveIndex { index, tracker }
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn into_index(self) -> Index {
        self.index
    }

    /// Re-reads the given (directory path, entry name) pairs from disk and
    /// updates the index to match. Returns true if the index changed.
    pub fn apply(&mut self, changes: &HashSet<(Vec<u8>, Vec<u8>)>) -> bool {
        self.tracker.apply(&mut self.index, changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, Query, SearchOptions};

    #[test]
    fn follows_creates_deletes_and_writes() {
        let root = std::env::temp_dir().join(format!("ff_live_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("keep")).unwrap();
        fs::write(root.join("keep/old.txt"), "1").unwrap();
        let root = crate::os::canonicalize(&root).unwrap();
        let mut live = LiveIndex::new(Index::build(&BuildOptions::new([&root])).unwrap());
        let root_bytes = crate::os::path_bytes(&root).to_vec();
        let change = |dir: &[u8], name: &str| HashSet::from([(dir.to_vec(), name.as_bytes().to_vec())]);
        let count = |live: &LiveIndex, q: &str| live.index().search(&Query::parse(q), &SearchOptions::default()).len();

        // A new file, and a new directory arriving with content.
        fs::write(root.join("fresh.txt"), "hello").unwrap();
        fs::create_dir_all(root.join("moved/inner")).unwrap();
        fs::write(root.join("moved/inner/deep.txt"), "x").unwrap();
        let mut batch = change(&root_bytes, "fresh.txt");
        batch.extend(change(&root_bytes, "moved"));
        assert!(live.apply(&batch));
        assert_eq!(count(&live, "fresh.txt"), 1);
        assert_eq!(count(&live, "deep.txt moved/inner/"), 1);

        // A file added inside the directory we just learned about.
        fs::write(root.join("moved/inner/late.txt"), "y").unwrap();
        assert!(live.apply(&change(&join(&join(&root_bytes, b"moved"), b"inner"), "late.txt")));
        assert_eq!(count(&live, "late.txt"), 1);

        // A write changes the size; an unchanged file reports no change.
        fs::write(root.join("fresh.txt"), "hello world").unwrap();
        assert!(live.apply(&change(&root_bytes, "fresh.txt")));
        assert!(!live.apply(&change(&root_bytes, "fresh.txt")));
        let id = live.index().search(&Query::parse("fresh.txt"), &SearchOptions::default())[0];
        assert_eq!(live.index().size(id), 11);

        // Removing a directory removes everything under it.
        let before = live.index().len();
        fs::remove_dir_all(root.join("moved")).unwrap();
        assert!(live.apply(&change(&root_bytes, "moved")));
        assert_eq!(live.index().len(), before - 4);
        assert_eq!(count(&live, "deep.txt"), 0);
        assert_eq!(count(&live, "old.txt keep/"), 1);

        // Unknown directories and vanished names are ignored.
        assert!(!live.apply(&change(b"/nonexistent/dir", "x")));
        assert!(!live.apply(&change(&root_bytes, "never-existed")));

        // The patched index still round-trips through a file.
        let db = root.join("index.db");
        live.index().save(&db).unwrap();
        assert_eq!(Index::load(&db).unwrap().len(), live.index().len());

        // A loaded index is mapped from its file; changing it must still
        // work, and must leave the file as it was.
        let saved = fs::read(&db).unwrap();
        let mut loaded = LiveIndex::new(Index::load(&db).unwrap());
        fs::write(root.join("after-load.txt"), "z").unwrap();
        assert!(loaded.apply(&change(&root_bytes, "after-load.txt")));
        assert_eq!(count(&loaded, "after-load.txt"), 1);
        assert_eq!(count(&loaded, "old.txt keep/"), 1);
        fs::remove_file(root.join("keep/old.txt")).unwrap();
        assert!(loaded.apply(&change(&join(&root_bytes, b"keep"), "old.txt")));
        assert_eq!(count(&loaded, "old.txt"), 0);
        assert_eq!(fs::read(&db).unwrap(), saved);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Patching in place (the window's way): ids stay valid, removed
    /// entries are marked gone, and a folder made again is found again.
    #[test]
    fn patches_in_place() {
        let root = std::env::temp_dir().join(format!("ff_tracker_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/f.txt"), "1").unwrap();
        let root = crate::os::canonicalize(&root).unwrap();
        let mut index = Index::build(&BuildOptions::new([&root])).unwrap();
        let mut tracker = Tracker::new(&index);
        let root_bytes = crate::os::path_bytes(&root).to_vec();
        let a = join(&root_bytes, b"a");
        let change = |dir: &[u8], name: &str| HashSet::from([(dir.to_vec(), name.as_bytes().to_vec())]);
        let find = |index: &Index, q: &str| index.search(&Query::parse(q), &SearchOptions::default());
        let f = find(&index, "f.txt")[0];
        let len = index.len();

        fs::remove_dir_all(root.join("a/b")).unwrap();
        assert!(tracker.apply(&mut index, &change(&a, "b")));
        assert_eq!(index.len(), len);
        assert!(index.is_gone(f) && find(&index, "f.txt").is_empty());

        fs::create_dir_all(root.join("a/b")).unwrap();
        assert!(tracker.apply(&mut index, &change(&a, "b")));
        fs::write(root.join("a/b/g.txt"), "2").unwrap();
        assert!(tracker.apply(&mut index, &change(&join(&a, b"b"), "g.txt")));
        assert_eq!(find(&index, "g.txt").len(), 1);
        assert_eq!(find(&index, "b").iter().filter(|&&id| index.is_dir(id)).count(), 1);
        fs::remove_dir_all(&root).unwrap();
    }
}

/// Times the tracker on a real index (FF_DB): `cargo test -p ff-core
/// tracker_timing -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn tracker_timing() {
    let db = std::env::var("FF_DB").unwrap();
    let t = std::time::Instant::now();
    let mut index = Index::load(&db).unwrap();
    eprintln!("load {} entries: {:?}", index.len(), t.elapsed());
    let t = std::time::Instant::now();
    let mut tracker = Tracker::new(&index);
    eprintln!("tracker: {} dirs in {:?}", tracker.dirs.len(), t.elapsed());
    let dir = crate::os::home_dir().unwrap().join("Documents");
    let name = format!("ff_timing_{}.txt", std::process::id());
    fs::write(dir.join(&name), "x").unwrap();
    let change = HashSet::from([(crate::os::path_bytes(&dir).to_vec(), name.as_bytes().to_vec())]);
    let t = std::time::Instant::now();
    let changed = tracker.apply(&mut index, &change);
    eprintln!("apply one new file: {changed} in {:?}", t.elapsed());
    fs::remove_file(dir.join(&name)).unwrap();
    let t = std::time::Instant::now();
    let changed = tracker.apply(&mut index, &change);
    eprintln!("apply its removal: {changed} in {:?}", t.elapsed());
}

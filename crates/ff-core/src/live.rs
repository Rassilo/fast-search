//! Keeping an index in step with the filesystem.
//!
//! A watcher reports *where* something changed as (directory, name) pairs;
//! [`LiveIndex::apply`] then looks at the disk to learn *what* is true now
//! and patches the index. Not interpreting event types keeps this correct
//! whatever the order or kind of events (create, delete, rename, write).

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use crate::index::{FLAG_DIR, Index};
use crate::walk::type_flags;

/// An index plus the lookup table needed to patch it quickly.
pub struct LiveIndex {
    index: Index,
    /// Full path of every indexed directory -> its entry id.
    dirs: HashMap<Vec<u8>, u32>,
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if !path.ends_with(b"/") {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

impl LiveIndex {
    pub fn new(index: Index) -> LiveIndex {
        let mut live = LiveIndex { index, dirs: HashMap::new() };
        live.rebuild_dirs();
        live
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    fn rebuild_dirs(&mut self) {
        self.dirs.clear();
        let mut path = Vec::new();
        for id in 0..self.index.len() as u32 {
            if self.index.flags[id as usize] == FLAG_DIR {
                self.index.write_path(id, &mut path);
                self.dirs.insert(path.clone(), id);
            }
        }
    }

    /// True if `path` is an excluded directory or inside one.
    fn excluded(&self, path: &[u8]) -> bool {
        self.index.excludes.iter().any(|x| {
            path.strip_prefix(&x[..]).is_some_and(|rest| rest.is_empty() || rest[0] == b'/' || x.ends_with(b"/"))
        })
    }

    /// Re-reads the given (directory path, entry name) pairs from disk and
    /// updates the index to match. Returns true if the index changed.
    pub fn apply(&mut self, changes: &HashSet<(Vec<u8>, Vec<u8>)>) -> bool {
        // Which names to look for under which indexed directory.
        let mut wanted: HashMap<u32, HashSet<&[u8]>> = HashMap::new();
        for (dir, name) in changes {
            if let Some(&id) = self.dirs.get(dir).filter(|_| !self.excluded(dir)) {
                wanted.entry(id).or_default().insert(name);
            }
        }
        if wanted.is_empty() {
            return false;
        }
        // One pass over the index finds the entries that already exist.
        let mut existing: HashMap<(u32, &[u8]), u32> = HashMap::new();
        for id in 0..self.index.len() as u32 {
            let parent = self.index.parent[id as usize];
            let name = self.index.name(id);
            if wanted.get(&parent).is_some_and(|names| names.contains(name)) {
                existing.insert((parent, name), id);
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
            self.index.write_path(dir, &mut dir_path);
            let path = join(&dir_path, &name);
            let md = fs::symlink_metadata(OsStr::from_bytes(&path)).ok();
            let flags = md.as_ref().map(|m| type_flags(m.file_type()));
            match (md, old) {
                // Still there, same kind: refresh size and time in place.
                (Some(md), Some(id)) if flags == Some(self.index.flags[id as usize]) => {
                    let i = id as usize;
                    changed |= (self.index.size[i], self.index.mtime[i]) != (md.size(), md.mtime());
                    (self.index.size[i], self.index.mtime[i]) = (md.size(), md.mtime());
                }
                (md, old) => {
                    changed |= md.is_some() || old.is_some();
                    dead.extend(old); // gone, or replaced by another kind
                    if let Some(md) = md {
                        self.add_tree(path, dir, &name, &md);
                    }
                }
            }
        }
        if !dead.is_empty() {
            // Children always follow their parent, so one ascending pass
            // carries a removal down to everything below it.
            let mut gone = vec![false; self.index.len()];
            for id in dead {
                gone[id as usize] = true;
            }
            for i in 0..gone.len() {
                let parent = self.index.parent[i] as usize;
                gone[i] = gone[i] || gone.get(parent).copied().unwrap_or(false);
            }
            self.index.compact(&gone);
            self.rebuild_dirs();
        }
        changed
    }

    /// Adds an entry and, for a directory, everything inside it.
    fn add_tree(&mut self, path: Vec<u8>, parent: u32, name: &[u8], md: &fs::Metadata) {
        let flags = type_flags(md.file_type());
        let id = self.index.push(name, parent, flags, md.size(), md.mtime());
        if flags != FLAG_DIR {
            return;
        }
        let mut stack = vec![(path, id)];
        while let Some((dir, dir_id)) = stack.pop() {
            self.dirs.insert(dir.clone(), dir_id);
            if self.excluded(&dir) {
                continue; // listed, but never descended into
            }
            let Ok(rd) = fs::read_dir(PathBuf::from(OsStr::from_bytes(&dir))) else { continue };
            for entry in rd.flatten() {
                let Ok(md) = entry.metadata() else { continue };
                let name = entry.file_name();
                let flags = type_flags(md.file_type());
                let id = self.index.push(name.as_bytes(), dir_id, flags, md.size(), md.mtime());
                if flags == FLAG_DIR {
                    stack.push((join(&dir, name.as_bytes()), id));
                }
            }
        }
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
        let root = fs::canonicalize(&root).unwrap();
        let mut live = LiveIndex::new(Index::build(&BuildOptions::new([&root])).unwrap());
        let root_bytes = root.as_os_str().as_bytes().to_vec();
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
        fs::remove_dir_all(&root).unwrap();
    }
}

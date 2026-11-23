//! Keeping the window's own index current as files change, when no ffd
//! daemon does it: the drives it covers are watched (see
//! `ff_core::watch`), and what changed is patched into the index in place,
//! so results show new, renamed and deleted files within a moment. The
//! index file is saved again once things are quiet and the window is
//! hidden, and when the program ends.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ff_core::Index;
use ff_core::live::Tracker;
use ff_core::watch::{Event, Watcher};

pub struct Follow {
    /// Held only to keep them running: dropping one stops it.
    _watchers: Vec<Watcher>,
    /// The roots watched, as the index names them.
    roots: Vec<Vec<u8>>,
    rx: mpsc::Receiver<Event>,
    pending: HashSet<(Vec<u8>, Vec<u8>)>,
    /// Built on first use, and again after the index is replaced.
    tracker: Option<Tracker>,
    /// The folder of the index file: our own writes there are not news.
    own: Vec<u8>,
    last_apply: Instant,
    last_change: Instant,
    /// When the first change not yet written came.
    unsaved_since: Instant,
    /// The index has changes the file does not.
    pub unsaved: bool,
    /// A watcher failed or could not cover everything; said once.
    pub lost: bool,
    /// Events were dropped: the index should be built again.
    pub overflow: bool,
}

/// What a check found.
pub enum Found {
    Nothing,
    /// The index changed.
    Changed,
    /// Events were lost: only rebuilding the index is safe.
    Rebuild,
}

pub fn roots(index: &Index) -> Vec<Vec<u8>> {
    index.roots().map(|r| index.name(r).to_vec()).collect()
}

impl Follow {
    /// Starts watching the roots of `index`; roots that cannot be watched
    /// are reported through `lost`.
    pub fn new(index: &Index, db: &Path) -> Follow {
        let (tx, rx) = mpsc::channel();
        let roots = roots(index);
        let excludes: Vec<Vec<u8>> = index.excludes().map(|p| ff_core::os::path_bytes(&p).to_vec()).collect();
        let mut lost = false;
        let watchers: Vec<Watcher> = roots
            .iter()
            .filter_map(|r| {
                let w = Watcher::start(PathBuf::from(ff_core::os::path_from_bytes(r)), excludes.clone(), tx.clone()).ok();
                lost |= w.is_none();
                w
            })
            .collect();
        let own = db.parent().map(|d| ff_core::os::path_bytes(d).to_vec()).unwrap_or_default();
        crate::debug::log(format!("follow: watching {} of {} roots", watchers.len(), roots.len()));
        Follow {
            _watchers: watchers,
            roots,
            rx,
            pending: HashSet::new(),
            tracker: None,
            own,
            last_apply: Instant::now(),
            last_change: Instant::now(),
            unsaved_since: Instant::now(),
            unsaved: false,
            lost,
            overflow: false,
        }
    }

    /// True if these watchers suit `index` (same roots).
    pub fn suits(&self, index: &Index) -> bool {
        self.roots == roots(index)
    }

    /// The index was replaced by a new one (built or loaded): what is known
    /// about the old one no longer holds. Changes seen meanwhile are kept;
    /// applying them again is harmless.
    pub fn replaced(&mut self) {
        self.tracker = None;
        self.unsaved = false;
    }

    /// Takes in what the watchers reported, and patches `index` if it is
    /// time: at once while the window shows (`eager`), else every few
    /// seconds.
    pub fn check(&mut self, index: &mut Index, eager: bool) -> Found {
        for event in self.rx.try_iter() {
            match event {
                Event::Changed(dir, name) => {
                    let own = !self.own.is_empty()
                        && dir.starts_with(&self.own)
                        && dir.get(self.own.len()).is_none_or(|&b| ff_core::os::is_sep(b));
                    if !own {
                        self.pending.insert((dir, name));
                    }
                }
                Event::Overflow => self.overflow = true,
                Event::Lost => self.lost = true,
            }
        }
        if std::mem::take(&mut self.overflow) {
            self.pending.clear();
            return Found::Rebuild;
        }
        let every = if eager { Duration::ZERO } else { Duration::from_secs(3) };
        if self.pending.is_empty() || self.last_apply.elapsed() < every {
            return Found::Nothing;
        }
        self.last_apply = Instant::now();
        let changes = std::mem::take(&mut self.pending);
        if self.apply(index, &changes) { Found::Changed } else { Found::Nothing }
    }

    /// Patches `index` for changes at these (directory, name) pairs.
    pub fn apply(&mut self, index: &mut Index, changes: &HashSet<(Vec<u8>, Vec<u8>)>) -> bool {
        let start = Instant::now();
        let tracker = self.tracker.get_or_insert_with(|| Tracker::new(index));
        let changed = tracker.apply(index, changes);
        if changed {
            if !self.unsaved {
                self.unsaved_since = Instant::now();
            }
            self.unsaved = true;
            self.last_change = Instant::now();
        }
        crate::debug::log(format!("follow: {} changes, index changed: {changed}, {:?}", changes.len(), start.elapsed()));
        if crate::debug::enabled() {
            for (dir, name) in changes.iter().take(8) {
                crate::debug::log(format!("  {} | {}", String::from_utf8_lossy(dir), String::from_utf8_lossy(name)));
            }
        }
        changed
    }

    /// True if the index should be written now: it has changes, and none
    /// came for a while (or, where files keep changing, every hour: the file
    /// is large, and writing it often would wear the disk).
    pub fn time_to_save(&self) -> bool {
        self.unsaved && (self.last_change.elapsed() >= Duration::from_secs(30) || self.unsaved_since.elapsed() >= Duration::from_secs(3600))
    }
}

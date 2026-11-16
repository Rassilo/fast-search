//! ff-core: instant file-name search.
//!
//! The whole file tree is kept in a compact in-memory [`Index`] (one name
//! arena plus a few flat arrays), built by a parallel directory walk and
//! persisted to a single file. Queries are a parallel linear scan over the
//! names, which stays in the low milliseconds for millions of files.
//!
//! ```no_run
//! use ff_core::{BuildOptions, Index, Query, SearchOptions};
//!
//! let index = Index::build(&BuildOptions::new(["/home"]))?;
//! index.save("/tmp/ff.db")?;
//! let hits = index.search(&Query::parse("report *.pdf"), &SearchOptions::default());
//! for id in hits {
//!     println!("{}", index.path(id).display());
//! }
//! # Ok::<(), std::io::Error>(())
//! ```

#[cfg(not(all(unix, target_pointer_width = "64")))]
compile_error!("ff-core supports 64-bit Unix targets only");

pub mod bookmarks;
pub mod complete;
pub mod desktop;
pub mod ffi;
mod index;
pub mod live;
mod query;
pub mod rawfs;
pub mod sizes;
pub mod time;
mod walk;

pub use index::{FLAG_DIR, FLAG_SYMLINK, Index, NO_PARENT};
pub use query::{CaseMode, Kind, Query, QueryOptions, SearchOptions, Sort};
pub use time::{local_civil, utc_civil};
pub use walk::BuildOptions;

use std::path::PathBuf;

/// Where the `ffd` daemon publishes its always-current index.
pub const SYSTEM_DB: &str = "/run/ff/index.db";

/// Default index location: `$FF_DB`; else the daemon's index if we may
/// read it; else `$XDG_CACHE_HOME/ff/index.db` or `~/.cache/ff/index.db`.
pub fn default_db_path() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(p) = var("FF_DB") {
        return Some(p.into());
    }
    if std::fs::File::open(SYSTEM_DB).is_ok() {
        return Some(SYSTEM_DB.into());
    }
    let cache = match var("XDG_CACHE_HOME") {
        Some(c) => PathBuf::from(c),
        None => PathBuf::from(var("HOME")?).join(".cache"),
    };
    Some(cache.join("ff/index.db"))
}

/// 1234567 -> "1,234,567".
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

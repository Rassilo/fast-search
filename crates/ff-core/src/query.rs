use std::ops::Range;
use std::thread;

use crate::index::Index;

/// How letter case is treated when matching.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Case-insensitive unless the term contains an uppercase letter.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

/// Restrict results to files or directories.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Any,
    /// Everything that is not a directory.
    File,
    Dir,
}

/// Result ordering.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Index order (fastest).
    #[default]
    None,
    Name,
    Path,
    Size,
    Mtime,
}

#[derive(Clone, Default)]
pub struct SearchOptions {
    pub kind: Kind,
    pub sort: Sort,
    pub reverse: bool,
    /// Maximum number of results; 0 means unlimited.
    pub limit: usize,
    /// Search threads; 0 picks a default from the CPU count.
    pub threads: usize,
}

struct Term {
    /// Pattern bytes, ASCII-lowercased when `ci`.
    pat: Vec<u8>,
    ci: bool,
    /// Match against the full path instead of the file name.
    path: bool,
    /// Wildcard pattern (`*`, `?`) that must match the whole name/path.
    glob: bool,
    negate: bool,
}

/// A parsed query: every term must match (AND).
///
/// Term syntax:
/// - `foo`      name contains "foo"
/// - `*.pdf`    wildcard, matches the whole name (`*` any run, `?` one byte)
/// - `src/main` contains a `/`: matched against the full path
/// - `!foo`     negation
///
/// Case folding is ASCII-only; other bytes match exactly.
#[derive(Default)]
pub struct Query {
    terms: Vec<Term>,
}

impl Query {
    /// Splits `text` on whitespace into terms; double quotes group a term
    /// containing spaces (`"my file"`).
    pub fn parse(text: &str) -> Query {
        Self::parse_with(text, CaseMode::Smart)
    }

    pub fn parse_with(text: &str, case: CaseMode) -> Query {
        let mut terms: Vec<Vec<u8>> = Vec::new();
        let mut cur = Vec::new();
        let mut quoted = false;
        for &b in text.as_bytes() {
            match b {
                b'"' => quoted = !quoted,
                b if b.is_ascii_whitespace() && !quoted => {
                    if !cur.is_empty() {
                        terms.push(std::mem::take(&mut cur));
                    }
                }
                b => cur.push(b),
            }
        }
        if !cur.is_empty() {
            terms.push(cur);
        }
        Self::from_terms(terms, case)
    }

    /// Builds a query from already-split terms (e.g. command-line arguments).
    pub fn from_terms<T: AsRef<[u8]>>(terms: impl IntoIterator<Item = T>, case: CaseMode) -> Query {
        let terms = terms
            .into_iter()
            .filter_map(|t| {
                let mut raw = t.as_ref();
                let negate = raw.len() > 1 && raw[0] == b'!';
                if negate {
                    raw = &raw[1..];
                }
                if raw.is_empty() {
                    return None;
                }
                let ci = match case {
                    CaseMode::Smart => !raw.iter().any(u8::is_ascii_uppercase),
                    CaseMode::Sensitive => false,
                    CaseMode::Insensitive => true,
                };
                Some(Term {
                    pat: if ci { raw.to_ascii_lowercase() } else { raw.to_vec() },
                    ci,
                    path: raw.contains(&b'/'),
                    glob: raw.iter().any(|&b| b == b'*' || b == b'?'),
                    negate,
                })
            })
            .collect();
        Query { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

impl Term {
    fn matches(&self, hay: &[u8]) -> bool {
        let hit = if self.glob {
            glob(&self.pat, hay, self.ci)
        } else {
            contains(hay, &self.pat, self.ci)
        };
        hit != self.negate
    }
}

#[inline]
fn fold(b: u8, ci: bool) -> u8 {
    if ci { b.to_ascii_lowercase() } else { b }
}

/// Substring test; `needle` is already lowercased when `ci`.
fn contains(hay: &[u8], needle: &[u8], ci: bool) -> bool {
    let Some((&first, rest)) = needle.split_first() else {
        return true;
    };
    if needle.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|i| {
        fold(hay[i], ci) == first
            && hay[i + 1..].iter().zip(rest).all(|(&h, &n)| fold(h, ci) == n)
    })
}

/// Whole-string wildcard match with single-star backtracking (linear memory,
/// no recursion). `pat` is already lowercased when `ci`.
fn glob(pat: &[u8], text: &[u8], ci: bool) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pat.len() && pat[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pat.len() && (pat[p] == b'?' || pat[p] == fold(text[t], ci)) {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            // Let the last star swallow one more byte and retry.
            star = Some((sp, st + 1));
            p = sp + 1;
            t = st + 1;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&b| b == b'*')
}

impl Index {
    /// Returns the ids of all entries matching `query`.
    pub fn search(&self, query: &Query, opts: &SearchOptions) -> Vec<u32> {
        let n = self.len();
        let threads = match opts.threads {
            0 => thread::available_parallelism().map_or(4, |n| n.get()),
            t => t,
        }
        .min(n / 32_768 + 1);

        let mut hits: Vec<u32> = if threads <= 1 {
            self.scan(query, opts.kind, 0..n)
        } else {
            thread::scope(|s| {
                let handles: Vec<_> = (0..threads)
                    .map(|t| {
                        let range = n * t / threads..n * (t + 1) / threads;
                        s.spawn(move || self.scan(query, opts.kind, range))
                    })
                    .collect();
                // Joining in spawn order keeps results in index order.
                handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
            })
        };

        match opts.sort {
            Sort::None => {}
            Sort::Name => hits.sort_by_cached_key(|&i| self.name(i).to_ascii_lowercase()),
            Sort::Path => hits.sort_by_cached_key(|&i| {
                let mut buf = Vec::new();
                self.write_path(i, &mut buf);
                buf
            }),
            Sort::Size => hits.sort_by_key(|&i| self.size(i)),
            Sort::Mtime => hits.sort_by_key(|&i| self.mtime(i)),
        }
        if opts.reverse {
            hits.reverse();
        }
        if opts.limit != 0 {
            hits.truncate(opts.limit);
        }
        hits
    }

    fn scan(&self, query: &Query, kind: Kind, range: Range<usize>) -> Vec<u32> {
        let mut out = Vec::new();
        let mut path = Vec::new();
        let has_path_terms = query.terms.iter().any(|t| t.path);
        for i in range {
            let id = i as u32;
            match kind {
                Kind::Any => {}
                Kind::File if self.is_dir(id) => continue,
                Kind::Dir if !self.is_dir(id) => continue,
                _ => {}
            }
            // Cheap name terms first; build the path only if they all pass.
            let name = self.name(id);
            if !query.terms.iter().filter(|t| !t.path).all(|t| t.matches(name)) {
                continue;
            }
            if has_path_terms {
                self.write_path(id, &mut path);
                if !query.terms.iter().filter(|t| t.path).all(|t| t.matches(&path)) {
                    continue;
                }
            }
            out.push(id);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BuildOptions;
    use std::fs;

    #[test]
    fn substring_and_glob() {
        assert!(contains(b"Report.PDF", b"port.p", true));
        assert!(!contains(b"Report.PDF", b"port.p", false));
        assert!(contains(b"abc", b"", true));
        assert!(!contains(b"ab", b"abc", true));
        assert!(glob(b"*.pdf", b"a.PDF", true));
        assert!(!glob(b"*.pdf", b"a.pdfx", true));
        assert!(glob(b"a*b*c", b"aXbbYc", false));
        assert!(glob(b"?.rs", b"a.rs", false));
        assert!(!glob(b"?.rs", b"ab.rs", false));
        assert!(glob(b"***", b"", false));
    }

    #[test]
    fn build_save_load_search() {
        let root = std::env::temp_dir().join(format!("ff_core_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("skipped")).unwrap();
        fs::write(root.join("src/Main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/deep/notes.txt"), "hello").unwrap();
        fs::write(root.join("my file.pdf"), "").unwrap();
        fs::write(root.join("skipped/hidden.rs"), "").unwrap();

        let mut opts = BuildOptions::new([&root]);
        opts.excludes.push(root.join("skipped"));
        let built = Index::build(&opts).unwrap();
        let db = root.join("index.db");
        built.save(&db).unwrap();
        let ix = Index::load(&db).unwrap();
        assert_eq!(ix.len(), built.len());

        let find = |q: &str, kind| {
            let o = SearchOptions { kind, sort: Sort::Path, ..Default::default() };
            let root = fs::canonicalize(&root).unwrap();
            ix.search(&Query::parse(q), &o)
                .into_iter()
                .map(|i| ix.path(i).strip_prefix(&root).unwrap().to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(find("main", Kind::Any), ["src/Main.rs"]);
        assert_eq!(find("Main", Kind::Any), ["src/Main.rs"]);
        assert!(find("MAIN", Kind::Any).is_empty()); // smart case
        assert_eq!(find("*.rs", Kind::Any), ["src/Main.rs"]); // skipped/ excluded
        assert_eq!(find("\"my file\"", Kind::Any), ["my file.pdf"]);
        assert_eq!(find("src/deep/ !deep", Kind::Any), ["src/deep/notes.txt"]);
        assert_eq!(find("dee", Kind::Dir), ["src/deep"]);
        // An excluded directory is listed itself, but never descended into.
        assert_eq!(find("skipped", Kind::Any), ["skipped"]);
        assert_eq!(find("txt", Kind::File), ["src/deep/notes.txt"]);

        let id = ix.search(&Query::parse("notes.txt"), &Default::default())[0];
        assert_eq!(ix.size(id), 5);
        assert!(!ix.is_dir(id));

        fs::write(&db, b"garbage").unwrap();
        assert!(Index::load(&db).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}

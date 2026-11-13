use std::env;
use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use ff_core::{BuildOptions, CaseMode, Index, Kind, Query, SearchOptions, Sort};

const USAGE: &str = "\
ff - fast find: instant file search

USAGE:
  ff [OPTIONS] TERM...          search (all terms must match)
  ff index [ROOT...] [-x DIR]   build or rebuild the index
  ff stats                      show index information

TERMS:
  foo        name contains \"foo\" (case-insensitive unless it has uppercase)
  '*.pdf'    wildcard on the whole name (* any run, ? one character)
  src/main   contains '/': matched against the full path
  '!foo'     must not match

SEARCH OPTIONS:
  -f, --files        files only
  -d, --dirs         directories only
  -c, --case         case-sensitive
  -i, --ignore-case  case-insensitive
  -n, --limit N      at most N results
  -s, --sort KEY     name | path | size | mtime
  -r, --reverse      reverse the order
  -l, --long         show size and modification time (UTC)
  -0, --null         separate results with NUL instead of newline
      --count        print only the number of matches
  -o, --open         open the first result with its default application
  -e, --edit         open the first result in the default text editor
      --open-folder  show the first result selected in the file manager
  -p, --properties   show the properties of the first result

INDEX OPTIONS:
  -x, --exclude DIR  skip a directory (repeatable)
  -j, --threads N    walker threads
  With no ROOT, the previous roots and excludes are reused (first run: /).

GENERAL:
      --db FILE      index file (default: $FF_DB, else
                     /run/ff/index.db when the ffd daemon runs,
                     else $XDG_CACHE_HOME/ff/index.db)
  -h, --help         -V, --version
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("ff: {msg}");
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct Args {
    free: Vec<OsString>,
    db: Option<PathBuf>,
    kind: Kind,
    case: CaseMode,
    sort: Sort,
    reverse: bool,
    limit: usize,
    long: bool,
    null: bool,
    count: bool,
    open: bool,
    open_folder: bool,
    edit: bool,
    properties: bool,
    excludes: Vec<PathBuf>,
    threads: usize,
}

fn run() -> Result<ExitCode, String> {
    let mut a = Args::default();
    let mut it = env::args_os().skip(1);
    let mut only_free = false;
    while let Some(arg) = it.next() {
        let Some(flag) = arg.to_str().filter(|s| !only_free && s.len() > 1 && s.starts_with('-')) else {
            a.free.push(arg);
            continue;
        };
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        let number = |v: OsString, name: &str| {
            v.to_str().and_then(|s| s.parse::<usize>().ok()).ok_or(format!("{name} needs a number"))
        };
        match flag {
            "--" => only_free = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            "-V" | "--version" => {
                println!("ff {}", env!("CARGO_PKG_VERSION"));
                return Ok(ExitCode::SUCCESS);
            }
            "-f" | "--files" => a.kind = Kind::File,
            "-d" | "--dirs" => a.kind = Kind::Dir,
            "-c" | "--case" => a.case = CaseMode::Sensitive,
            "-i" | "--ignore-case" => a.case = CaseMode::Insensitive,
            "-r" | "--reverse" => a.reverse = true,
            "-l" | "--long" => a.long = true,
            "-0" | "--null" => a.null = true,
            "--count" => a.count = true,
            "-o" | "--open" => a.open = true,
            "--open-folder" => a.open_folder = true,
            "-e" | "--edit" => a.edit = true,
            "-p" | "--properties" => a.properties = true,
            "-n" | "--limit" => a.limit = number(value(flag)?, flag)?,
            "-j" | "--threads" => a.threads = number(value(flag)?, flag)?,
            "-x" | "--exclude" => a.excludes.push(value(flag)?.into()),
            "--db" => a.db = Some(value(flag)?.into()),
            "-s" | "--sort" => {
                a.sort = match value(flag)?.to_str() {
                    Some("name") => Sort::Name,
                    Some("path") => Sort::Path,
                    Some("size") => Sort::Size,
                    Some("mtime") => Sort::Mtime,
                    _ => return Err("--sort takes name, path, size or mtime".into()),
                }
            }
            _ => return Err(format!("unknown option {flag} (see ff --help)")),
        }
    }

    let db = a.db.take().map_or_else(default_db, Ok)?;
    // A leading "index"/"stats" is a command unless "--" was used.
    match a.free.first().and_then(|f| f.to_str()).filter(|_| !only_free) {
        Some("index") => cmd_index(&a, &db),
        Some("stats") => cmd_stats(&db),
        Some("help") => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        _ if a.free.is_empty() => {
            eprint!("{USAGE}");
            Ok(ExitCode::from(2))
        }
        _ => cmd_search(&a, &db),
    }
}

fn default_db() -> Result<PathBuf, String> {
    ff_core::default_db_path().ok_or_else(|| "HOME is not set; use --db".into())
}

fn load(db: &PathBuf) -> Result<Index, String> {
    Index::load(db).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("no index at {}; run `ff index` first", db.display()),
        _ => format!("{}: {e}", db.display()),
    })
}

fn cmd_index(a: &Args, db: &PathBuf) -> Result<ExitCode, String> {
    if db.as_os_str() == ff_core::SYSTEM_DB {
        return Err(format!("{} is kept up to date by the ffd daemon; use --db FILE for a private index", db.display()));
    }
    let roots: Vec<PathBuf> = a.free[1..].iter().map(PathBuf::from).collect();
    let mut opts = if !roots.is_empty() {
        BuildOptions::new(roots)
    } else {
        // Refresh: same roots and excludes as last time (first run: /).
        BuildOptions::refresh(&Index::load(db).unwrap_or_default())
    };
    for x in &a.excludes {
        if !opts.excludes.contains(x) {
            opts.excludes.push(x.clone());
        }
    }
    opts.threads = a.threads;

    let start = Instant::now();
    let index = Index::build(&opts).map_err(|e| e.to_string())?;
    index.save(db).map_err(|e| format!("{}: {e}", db.display()))?;
    eprintln!(
        "indexed {} entries in {:.2}s -> {}",
        index.len(),
        start.elapsed().as_secs_f64(),
        db.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_stats(db: &PathBuf) -> Result<ExitCode, String> {
    let index = load(db)?;
    let dirs = (0..index.len() as u32).filter(|&i| index.is_dir(i)).count();
    let file_len = std::fs::metadata(db).map_or(0, |m| m.len());
    println!("index:    {} ({:.1} MiB)", db.display(), file_len as f64 / 1048576.0);
    println!("entries:  {} ({} directories, {} files)", index.len(), dirs, index.len() - dirs);
    println!("memory:   {:.1} MiB", index.memory_usage() as f64 / 1048576.0);
    for r in index.roots() {
        println!("root:     {}", index.path(r).display());
    }
    for x in index.excludes() {
        println!("exclude:  {}", x.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_search(a: &Args, db: &PathBuf) -> Result<ExitCode, String> {
    let index = load(db)?;
    let query = Query::from_terms(a.free.iter().map(|t| t.as_bytes()), a.case);
    let opts = SearchOptions {
        kind: a.kind,
        sort: a.sort,
        reverse: a.reverse,
        limit: a.limit,
        threads: 0,
    };
    let hits = index.search(&query, &opts);
    let found = if hits.is_empty() { ExitCode::from(1) } else { ExitCode::SUCCESS };

    if (a.open || a.open_folder || a.edit || a.properties) && !hits.is_empty() {
        let target = index.path(hits[0]);
        if a.properties {
            let props = ff_core::desktop::properties(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            for (label, value) in props {
                println!("{:<12} {value}", format!("{label}:"));
            }
            return Ok(ExitCode::SUCCESS);
        }
        println!("{}", target.display());
        let done = if a.open_folder {
            ff_core::desktop::reveal(&target)
        } else if a.edit {
            ff_core::desktop::open_in_editor(&target)
        } else {
            ff_core::desktop::open(&target)
        };
        return done.map(|()| ExitCode::SUCCESS).map_err(|e| e.to_string());
    }

    // A closed pipe (e.g. `ff foo | head`) just ends the output.
    let _ = print_hits(a, &index, &hits);
    Ok(found)
}

fn print_hits(a: &Args, index: &Index, hits: &[u32]) -> io::Result<()> {
    let mut out = BufWriter::with_capacity(1 << 16, io::stdout().lock());
    if a.count {
        writeln!(out, "{}", hits.len())?;
        return out.flush();
    }
    let mut path = Vec::new();
    for &id in hits {
        if a.long {
            let (y, mo, d, h, mi) = ff_core::utc_civil(index.mtime(id));
            let kind = if index.is_dir(id) { 'd' } else if index.is_symlink(id) { 'l' } else { '-' };
            write!(out, "{kind} {:>12} {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}  ", index.size(id))?;
        }
        index.write_path(id, &mut path);
        out.write_all(&path)?;
        out.write_all(if a.null { b"\0" } else { b"\n" })?;
    }
    out.flush()
}

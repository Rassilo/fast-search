//! ff-gui: a search-as-you-type window over the ff index.

mod canvas;
mod settings;
mod x11;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use canvas::{Canvas, Icon, Icons, Text};
use ff_core::{BuildOptions, CaseMode, Index, Kind, Query, SearchOptions, Sort, desktop, group_digits};
use settings::Config;
use x11::P::*;
use x11::*;

const USAGE: &str = "\
Fast search (ff-gui) - instant file search window

USAGE:
  ff-gui [--db FILE] [QUERY...]
  ff-gui --install     add Fast search to the applications menu, with its icon
  ff-gui --shortcut on|off   make Alt+E open Fast search (GNOME), or undo it

KEYS:
  type               search as you type (same term syntax as `ff`)
  Up/Down/PgUp/PgDn, Ctrl+Home/End   move through the results
  Left/Right/Home/End   move in the search box (Shift selects, Ctrl by word)
  Ctrl+A / Ctrl+X    select all / cut the selected search text
  Enter              open the selected file         (double-click too)
  Ctrl+Enter         show it in the file manager    (double-click its path)
  Alt+Enter          properties
  F4                 open in the default text editor
  Ctrl+C             copy the selected search text, else the file itself
                     (paste it in a file manager; as text it is the path)
  Ctrl+Shift+C       copy the full path
  Ctrl+V             paste into the search box
  Menu / right-click actions menu
  menu bar           File, Edit, View, Options and Help at the top
  Ctrl + / - / 0     zoom in / out / reset
  F2                 show all / files only / folders only
  F3                 case: smart / sensitive / insensitive
  F5                 rebuild the index (not needed while ffd runs)
  click a column     sort by it (again to reverse)
  Esc                clear the search box, or quit when it is empty

ENVIRONMENT:
  FF_GUI_SCALE       zoom factor, e.g. 1.5 (default: the desktop scale)
  FF_GUI_FONT        font file to use (default: the system sans-serif)
  FF_GUI_TITLEBAR    set to system to use the desktop title bar
                     instead of the built-in one (which shows the icon)
";

/// Text height in pixels at 100% scale.
const FONT_PX: f32 = 14.5;

const WHITE: u32 = 0xffffff;
const PANEL: u32 = 0xf2f2f2;
const LINE: u32 = 0xd0d0d0;
const TEXT: u32 = 0x1f1f1f;
const DIM: u32 = 0x666666;
const SELECT: u32 = 0x1a66c9;
const HOVER: u32 = 0xdbe9fa;

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Open,
    Reveal,
    /// Open in the default text editor.
    Edit,
    /// Copy the file itself, to paste it in a file manager.
    CopyFile,
    CopyPath,
    CopyName,
    Properties,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Show(Kind),
    Case(CaseMode),
    Reindex,
    About,
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleTitleBar,
    ToggleShortcut,
    ToggleLauncher,
    ToggleDaemon,
}

/// Menu entries: label, shortcut hint, action.
type Items = &'static [(&'static str, &'static str, Action)];

/// The right-click menu of a result.
const CONTEXT_MENU: Items = &[
    ("Open", "Enter", Action::Open),
    ("Open folder", "Ctrl+Enter", Action::Reveal),
    ("Copy", "Ctrl+C", Action::CopyFile),
    ("Copy full path", "Ctrl+Shift+C", Action::CopyPath),
    ("Copy name", "", Action::CopyName),
    ("Properties", "Alt+Enter", Action::Properties),
];

/// The same, for files that make sense in a text editor.
const TEXT_CONTEXT_MENU: Items = &[
    ("Open", "Enter", Action::Open),
    ("Open in text editor", "F4", Action::Edit),
    ("Open folder", "Ctrl+Enter", Action::Reveal),
    ("Copy", "Ctrl+C", Action::CopyFile),
    ("Copy full path", "Ctrl+Shift+C", Action::CopyPath),
    ("Copy name", "", Action::CopyName),
    ("Properties", "Alt+Enter", Action::Properties),
];

/// The menu bar.
const BAR: [(&str, Items); 5] = [
    (
        "File",
        &[
            ("Open", "Enter", Action::Open),
            ("Open in text editor", "F4", Action::Edit),
            ("Open folder", "Ctrl+Enter", Action::Reveal),
            ("Copy full path", "Ctrl+Shift+C", Action::CopyPath),
            ("Copy name", "", Action::CopyName),
            ("Properties", "Alt+Enter", Action::Properties),
            ("Rebuild index", "F5", Action::Reindex),
            ("Quit", "Ctrl+Q", Action::Quit),
        ],
    ),
    (
        "Edit",
        &[
            ("Cut", "Ctrl+X", Action::Cut),
            ("Copy", "Ctrl+C", Action::Copy),
            ("Paste", "Ctrl+V", Action::Paste),
            ("Select all", "Ctrl+A", Action::SelectAll),
        ],
    ),
    (
        "View",
        &[
            ("All results", "F2", Action::Show(Kind::Any)),
            ("Files only", "", Action::Show(Kind::File)),
            ("Folders only", "", Action::Show(Kind::Dir)),
            ("Smart case", "F3", Action::Case(CaseMode::Smart)),
            ("Case sensitive", "", Action::Case(CaseMode::Sensitive)),
            ("Case insensitive", "", Action::Case(CaseMode::Insensitive)),
        ],
    ),
    (
        "Options",
        &[
            ("Zoom in", "Ctrl++", Action::ZoomIn),
            ("Zoom out", "Ctrl+-", Action::ZoomOut),
            ("Reset zoom", "Ctrl+0", Action::ZoomReset),
            ("Title bar with icon", "", Action::ToggleTitleBar),
            ("Global shortcut Alt+E", "", Action::ToggleShortcut),
            ("Show in applications menu", "", Action::ToggleLauncher),
            ("Live updates service", "asks for password", Action::ToggleDaemon),
        ],
    ),
    ("Help", &[("About Fast search", "", Action::About)]),
];

/// An open menu: a result's right-click menu or one from the menu bar.
struct Menu {
    x: i32,
    y: i32,
    hover: Option<usize>,
    items: Items,
    /// Which menu-bar title it hangs from, if any.
    bar: Option<usize>,
}

/// Desktop scale: FF_GUI_SCALE, else the Xft.dpi resource (96 dpi = 1.0).
fn desktop_scale(c: &mut Conn) -> f32 {
    let env = std::env::var("FF_GUI_SCALE").ok().and_then(|v| v.parse::<f32>().ok());
    let scale = env.or_else(|| {
        let rm = c.get_property(c.root, 23, false).ok()?; // RESOURCE_MANAGER
        let rm = String::from_utf8_lossy(&rm).into_owned();
        let dpi = rm.lines().find_map(|l| l.strip_prefix("Xft.dpi:"))?.trim().parse::<f32>().ok()?;
        Some(dpi / 96.0)
    });
    scale.unwrap_or(1.0).clamp(0.5, 8.0)
}

struct App {
    c: Conn,
    win: u32,
    gc: u32,
    cv: Canvas,
    /// The frame the window currently shows (empty: must resend all).
    shown: Vec<u8>,
    text: Text,
    icons: Icons,
    /// Pixels per layout unit (the rounded desktop scale).
    u: i32,
    size_w: i32,
    date_w: i32,

    db: PathBuf,
    /// Modification time of the index file when we loaded it.
    db_stamp: Option<SystemTime>,
    index: Index,
    query: String,
    /// Caret and selection anchor in the search box (byte offsets).
    cursor: usize,
    anchor: usize,
    /// Pixels the search text is scrolled left.
    box_scroll: i32,
    selecting: bool,
    last_box_click: u32,
    hits: Vec<u32>,
    sel: usize,
    top: usize,
    kind: Kind,
    case: CaseMode,
    sort: Sort,
    reverse: bool,
    status: String,
    menu: Option<Menu>,
    /// Open properties panel: label/value rows.
    props: Option<Vec<(&'static str, String)>>,
    about: bool,
    about_icon: Icons,
    quit: bool,

    /// Current zoom, and the desktop's own scale that "reset" returns to.
    scale: f32,
    auto_scale: f32,
    /// Follow the default index location (no --db given).
    follow_default: bool,
    /// Results of background jobs, shown in the status bar.
    notices: mpsc::Receiver<String>,
    notice_tx: mpsc::Sender<String>,
    /// State of the Options toggles, refreshed when the menu opens.
    shortcut_on: bool,
    launcher_on: bool,
    daemon_on: bool,
    motif_hints: u32,
    /// We draw the title bar ourselves (so it can show the app icon).
    custom_title: bool,
    /// Title-bar button under the pointer: 0 minimize, 1 maximize, 2 close.
    hover_button: Option<usize>,
    last_title_click: u32,
    /// Pointer position on the screen at the last button press.
    pointer_root: (i32, i32),
    net_wm_moveresize: u32,
    net_wm_state: u32,
    net_wm_maximized: [u32; 2],
    wm_change_state: u32,
    dragging_scrollbar: bool,

    clipboard: u32,
    utf8_string: u32,
    wm_delete: u32,
    paste_prop: u32,
    copied: String,
    /// The copied text is the path of a copied file.
    copied_file: bool,
    clip_atoms: ClipAtoms,
    /// Our copy is the current clipboard content.
    owns_clipboard: bool,
    last_click: (u32, usize),
}

fn main() -> ExitCode {
    let mut db = None;
    let mut query = Vec::new();
    let mut args = std::env::args();
    args.next();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("ff-gui {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--db" => db = args.next().map(PathBuf::from),
            "--shortcut" => {
                let on = match args.next().as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => {
                        eprintln!("ff-gui: --shortcut takes on or off");
                        return ExitCode::from(2);
                    }
                };
                return match settings::set_shortcut(on) {
                    Ok(true) => {
                        println!("Alt+E now opens Fast search");
                        ExitCode::SUCCESS
                    }
                    Ok(false) => {
                        println!("global shortcut removed");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("ff-gui: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            "--hold-clipboard" => {
                return match hold_clipboard(args.next().as_deref() == Some("file")) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(_) => ExitCode::FAILURE,
                };
            }
            "--install" => {
                return match settings::install_launcher() {
                    Ok(file) => {
                        println!("installed {}", file.display());
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("ff-gui: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            _ => query.push(a),
        }
    }
    let follow_default = db.is_none();
    let Some(db) = db.or_else(ff_core::default_db_path) else {
        eprintln!("ff-gui: HOME is not set; use --db");
        return ExitCode::from(2);
    };
    match App::new(db, follow_default, query.join(" ")).and_then(|mut app| app.run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ff-gui: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The atoms used to talk about the clipboard.
#[derive(Clone, Copy)]
struct ClipAtoms {
    clipboard: u32,
    targets: u32,
    utf8_string: u32,
    /// "text/uri-list": the standard format for copied files.
    uri_list: u32,
    /// GNOME's own format for copied files.
    gnome_files: u32,
}

impl ClipAtoms {
    fn intern(c: &mut Conn) -> io::Result<ClipAtoms> {
        Ok(ClipAtoms {
            clipboard: c.intern_atom("CLIPBOARD")?,
            targets: c.intern_atom("TARGETS")?,
            utf8_string: c.intern_atom("UTF8_STRING")?,
            uri_list: c.intern_atom("text/uri-list")?,
            gnome_files: c.intern_atom("x-special/gnome-copied-files")?,
        })
    }
}

/// Answers another application's request (a SelectionRequest event) for
/// the clipboard text we own.
/// `file` says the text is the path of a copied file, which is then also
/// offered in the formats file managers paste from.
fn serve_selection(c: &mut Conn, ev: &[u8], text: &str, file: bool, atoms: &ClipAtoms) {
    let ClipAtoms { targets, utf8_string, uri_list, gnome_files, .. } = *atoms;
    let uri = ff_core::desktop::file_uri(std::path::Path::new(text));
    let (requestor, selection, target) = (u32at(ev, 12), u32at(ev, 16), u32at(ev, 20));
    let mut property = match u32at(ev, 24) {
        0 => target, // obsolete clients
        p => p,
    };
    if target == targets {
        let mut list = vec![targets, utf8_string, ATOM_STRING];
        if file {
            list.extend([uri_list, gnome_files]);
        }
        let list: Vec<u8> = list.iter().flat_map(|a| a.to_le_bytes()).collect();
        c.set_property(requestor, property, ATOM_ATOM, 32, &list);
    } else if file && target == uri_list {
        c.set_property(requestor, property, target, 8, format!("{uri}\r\n").as_bytes());
    } else if file && target == gnome_files {
        c.set_property(requestor, property, target, 8, format!("copy\n{uri}").as_bytes());
    } else if target == utf8_string || target == ATOM_STRING {
        c.set_property(requestor, property, target, 8, text.as_bytes());
    } else {
        property = 0; // refused
    }
    // SendEvent(SelectionNotify) back to the requestor.
    let event = [
        W(requestor),
        W(0),
        B(EV_SELECTION_NOTIFY),
        B(0),
        H(0),
        W(u32at(ev, 4)),
        W(requestor),
        W(selection),
        W(target),
        W(property),
        S(&[0; 8]),
    ];
    c.req(25, 0, &event);
}

/// Takes ownership of the clipboard for `window`; true if we got it.
fn own_clipboard(c: &mut Conn, window: u32, clipboard: u32) -> io::Result<bool> {
    // Time 0 means "now". A key-press timestamp can be refused: under
    // XWayland it comes from the compositor's clock, not the X server's.
    c.req(22, 0, &[W(window), W(clipboard), W(0)]); // SetSelectionOwner
    let seq = c.req(23, 0, &[W(clipboard)]); // GetSelectionOwner
    Ok(c.reply(seq)?.is_some_and(|r| u32at(&r, 8) == window))
}

/// `--hold-clipboard text|file`: keeps text (read from stdin) on the
/// clipboard after the window that copied it has closed, until something
/// else is copied. `file` means the text is the path of a copied file.
/// X11 clipboards live in the owning program, so without this a copy
/// would vanish the moment Fast search quits.
fn hold_clipboard(file: bool) -> io::Result<()> {
    let mut text = String::new();
    io::Read::read_to_string(&mut io::stdin(), &mut text)?;
    let mut c = Conn::connect()?;
    let win = c.new_id();
    // An invisible 1x1 window, never mapped, just to own the selection.
    c.req(1, 0, &[W(win), W(c.root), H(0), H(0), H(1), H(1), H(0), H(1), W(0), W(0)]);
    let atoms = ClipAtoms::intern(&mut c)?;
    if !own_clipboard(&mut c, win, atoms.clipboard)? {
        return Ok(());
    }
    loop {
        let Some(ev) = c.next_event(None)? else { continue };
        match ev[0] & 0x7f {
            EV_SELECTION_REQUEST => serve_selection(&mut c, &ev, &text, file, &atoms),
            EV_SELECTION_CLEAR => return Ok(()), // someone else copied
            _ => {}
        }
    }
}

impl App {
    fn new(db: PathBuf, follow_default: bool, query: String) -> io::Result<App> {
        let mut c = Conn::connect()?;
        let config = Config::load();
        let (notice_tx, notices) = mpsc::channel();
        // FF_GUI_SCALE beats the saved zoom, which beats the desktop scale.
        let auto_scale = desktop_scale(&mut c);
        let scale = config.scale.filter(|_| std::env::var_os("FF_GUI_SCALE").is_none()).unwrap_or(auto_scale);
        let u = (scale.round() as i32).max(1);
        let mut text = Text::load((FONT_PX * scale).round())?;

        let (w, h) = (1000 * u, 640 * u);
        let win = c.new_id();
        // Events: KeyPress | ButtonPress | ButtonRelease | PointerMotion |
        // Exposure | StructureNotify.
        let events = 0x1 | 0x4 | 0x8 | 0x40 | 0x8000 | 0x20000;
        c.req(
            1,
            0,
            &[W(win), W(c.root), H(0), H(0), H(w as u16), H(h as u16), H(0), H(1), W(0), W(0x800), W(events)],
        );
        let gc = c.new_id();
        c.req(55, 0, &[W(gc), W(win), W(0x10000), W(0)]); // no graphics exposures

        let wm_protocols = c.intern_atom("WM_PROTOCOLS")?;
        let wm_delete = c.intern_atom("WM_DELETE_WINDOW")?;
        let utf8_string = c.intern_atom("UTF8_STRING")?;
        let net_wm_name = c.intern_atom("_NET_WM_NAME")?;
        c.set_property(win, wm_protocols, ATOM_ATOM, 32, &wm_delete.to_le_bytes());
        c.set_property(win, ATOM_WM_NAME, ATOM_STRING, 8, b"Fast search");
        c.set_property(win, net_wm_name, utf8_string, 8, b"Fast search");
        // _NET_WM_ICON: width, height, then ARGB pixels, for each size.
        let net_wm_icon = c.intern_atom("_NET_WM_ICON")?;
        let mut icon = Vec::new();
        for size in [48usize, 128] {
            icon.extend((size as u32).to_le_bytes());
            icon.extend((size as u32).to_le_bytes());
            icon.extend(Icon::App.render(size).into_iter().flatten());
        }
        c.set_property(win, net_wm_icon, 6, 32, &icon); // type CARDINAL

        // Ask the window manager for no title bar or frame; we draw ours.
        let custom_title = std::env::var("FF_GUI_TITLEBAR").as_deref() != Ok("system") && config.own_title_bar;
        let motif_hints = c.intern_atom("_MOTIF_WM_HINTS")?;
        c.set_property(win, ATOM_WM_CLASS, ATOM_STRING, 8, b"ff-gui\0ff-gui\0");

        let clip_atoms = ClipAtoms::intern(&mut c)?;
        let net_wm_moveresize = c.intern_atom("_NET_WM_MOVERESIZE")?;
        let net_wm_state = c.intern_atom("_NET_WM_STATE")?;
        let net_wm_maximized =
            [c.intern_atom("_NET_WM_STATE_MAXIMIZED_VERT")?, c.intern_atom("_NET_WM_STATE_MAXIMIZED_HORZ")?];
        let wm_change_state = c.intern_atom("WM_CHANGE_STATE")?;

        let mut app = App {
            clipboard: c.intern_atom("CLIPBOARD")?,
            paste_prop: c.intern_atom("FF_PASTE")?,
            utf8_string,
            wm_delete,
            c,
            win,
            gc,
            cv: Canvas::new(),
            shown: Vec::new(),
            size_w: text.width("000,000,000 KB") + 16 * u,
            date_w: text.width("0000-00-00 00:00") + 16 * u,
            text,
            icons: Icons::new((16 * u) as usize),
            u,
            index: Index::load(&db).unwrap_or_default(),
            db_stamp: std::fs::metadata(&db).and_then(|m| m.modified()).ok(),
            db,
            cursor: query.len(),
            anchor: query.len(),
            box_scroll: 0,
            selecting: false,
            last_box_click: 0,
            query,
            hits: Vec::new(),
            sel: 0,
            top: 0,
            kind: Kind::Any,
            case: CaseMode::Smart,
            sort: Sort::None,
            reverse: false,
            status: String::new(),
            menu: None,
            props: None,
            about: false,
            about_icon: Icons::new((72 * u) as usize),
            quit: false,
            scale,
            auto_scale,
            follow_default,
            notices,
            notice_tx,
            shortcut_on: false,
            launcher_on: false,
            daemon_on: false,
            motif_hints,
            custom_title,
            hover_button: None,
            last_title_click: 0,
            pointer_root: (0, 0),
            net_wm_moveresize,
            net_wm_state,
            net_wm_maximized,
            wm_change_state,
            dragging_scrollbar: false,
            copied: String::new(),
            copied_file: false,
            clip_atoms,
            owns_clipboard: false,
            last_click: (0, usize::MAX),
        };
        app.apply_title_bar();
        app.cv.resize(w as usize, h as usize);
        app.c.req(8, 0, &[W(win)]); // MapWindow
        Ok(app)
    }

    fn run(&mut self) -> io::Result<()> {
        let result = self.event_loop();
        self.hand_over_clipboard();
        result
    }

    fn event_loop(&mut self) -> io::Result<()> {
        if self.index.is_empty() {
            self.reindex()?; // first start: no index yet
        }
        self.search();
        // Test hooks: open a panel, save the first frame, exit.
        match std::env::var("FF_GUI_TEST").as_deref() {
            Ok("menu") => {
                let (x, y) = (300 * self.u, 150 * self.u);
                self.menu = Some(Menu { x, y, hover: Some(1), items: self.context_items(), bar: None });
            }
            Ok("bar") => {
                self.open_bar_menu(2);
                self.menu.as_mut().unwrap().hover = Some(1);
            }
            Ok("about") => self.about = true,
            // Copy as a real key press would, then quit: the copy must survive.
            Ok("copy") => self.act(Action::CopyFile, 123_456_789),
            Ok("copy-quit") => {
                self.act(Action::CopyFile, 123_456_789);
                return Ok(());
            }
            Ok("options") => self.open_bar_menu(3),
            Ok("props") => self.act(Action::Properties, 0),
            Ok("select") => self.anchor = self.step(self.cursor, false, true),
            _ => {}
        }
        let mut dirty = true;
        loop {
            if self.quit {
                return Ok(());
            }
            while let Ok(notice) = self.notices.try_recv() {
                self.status = notice;
                dirty = true;
            }
            // Redraw only once the event queue is drained.
            // While idle, wake once a second to notice a refreshed index.
            let wait = if dirty { Duration::ZERO } else { Duration::from_secs(1) };
            let Some(ev) = self.c.next_event(Some(wait))? else {
                if !dirty {
                    dirty = self.reload_if_changed();
                    continue;
                }
                self.render()?;
                if let Some(path) = std::env::var_os("FF_GUI_SNAPSHOT") {
                    let mut ppm = format!("P6 {} {} 255\n", self.cv.w, self.cv.h).into_bytes();
                    ppm.extend(self.cv.px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]));
                    return std::fs::write(path, ppm);
                }
                dirty = false;
                continue;
            };
            let at = |i| u16at(&ev, i) as i16 as i32;
            match ev[0] & 0x7f {
                EV_EXPOSE => {
                    self.shown.clear();
                    dirty = true;
                }
                EV_CONFIGURE => {
                    let (w, h) = (u16at(&ev, 20) as usize, u16at(&ev, 22) as usize);
                    if (w, h) != (self.cv.w, self.cv.h) {
                        self.cv.resize(w.max(1), h.max(1));
                        self.shown.clear();
                        self.menu = None;
                        dirty = true;
                    }
                }
                EV_KEY_PRESS => {
                    if !self.key(ev[1], u16at(&ev, 28), u32at(&ev, 4))? {
                        return Ok(());
                    }
                    dirty = true;
                }
                EV_BUTTON_PRESS => {
                    self.pointer_root = (at(20), at(22));
                    self.press(ev[1], at(24), at(26), u32at(&ev, 4), u16at(&ev, 28));
                    dirty = true;
                }
                EV_BUTTON_RELEASE if ev[1] == 1 => {
                    self.dragging_scrollbar = false;
                    self.selecting = false;
                }
                EV_MOTION => {
                    let hover = self.title_button_at(at(24), at(26));
                    dirty |= std::mem::replace(&mut self.hover_button, hover) != hover;
                    dirty |= self.motion(at(24), at(26));
                }
                EV_SELECTION_REQUEST => {
                    let (text, atoms) = (self.copied.clone(), self.clip_atoms);
                    serve_selection(&mut self.c, &ev, &text, self.copied_file, &atoms);
                }
                EV_SELECTION_CLEAR => self.owns_clipboard = false, // someone else copied
                EV_SELECTION_NOTIFY if u32at(&ev, 20) != 0 => {
                    let data = self.c.get_property(self.win, self.paste_prop, true)?;
                    let text = String::from_utf8_lossy(&data);
                    let line: String = text.lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).collect();
                    self.insert(&line);
                    dirty = true;
                }
                EV_CLIENT_MESSAGE if u32at(&ev, 12) == self.wm_delete => return Ok(()),
                EV_MAPPING => self.c.load_keymap()?,
                _ => {}
            }
        }
    }

    // ---- state ---------------------------------------------------------

    fn search(&mut self) {
        let opts = SearchOptions {
            kind: self.kind,
            sort: self.sort,
            reverse: self.reverse,
            ..Default::default()
        };
        self.hits = self.index.search(&Query::parse_with(&self.query, self.case), &opts);
        self.sel = 0;
        self.top = 0;
    }

    // ---- search box editing -------------------------------------------

    /// The selected byte range of the search text (empty if none).
    fn selection(&self) -> (usize, usize) {
        (self.cursor.min(self.anchor), self.cursor.max(self.anchor))
    }

    /// Removes the selected text; returns false if nothing was selected.
    fn delete_selection(&mut self) -> bool {
        let (a, b) = self.selection();
        self.query.replace_range(a..b, "");
        (self.cursor, self.anchor) = (a, a);
        a != b
    }

    /// Replaces the selection with `s` and searches again.
    fn insert(&mut self, s: &str) {
        self.delete_selection();
        self.query.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.anchor = self.cursor;
        self.search();
    }

    fn clear_query(&mut self) {
        self.query.clear();
        (self.cursor, self.anchor) = (0, 0);
        self.search();
    }

    /// Caret position one character or one word to the left or right.
    fn step(&self, from: usize, right: bool, word: bool) -> usize {
        let (before, after) = self.query.split_at(from);
        match (right, word) {
            (false, false) => before.chars().next_back().map_or(0, |c| from - c.len_utf8()),
            (true, false) => after.chars().next().map_or(from, |c| from + c.len_utf8()),
            (false, true) => {
                let word = before.trim_end();
                word.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8())
            }
            (true, true) => {
                let word = after.trim_start();
                self.query.len() - word.len() + word.find(char::is_whitespace).unwrap_or(word.len())
            }
        }
    }

    /// Moves the caret, extending the selection if `select`.
    fn move_caret(&mut self, to: usize, select: bool) {
        self.cursor = to;
        if !select {
            self.anchor = to;
        }
    }

    /// Deletes the selection, or else from the caret to `to`.
    fn delete_to(&mut self, to: usize) {
        if self.selection().0 == self.selection().1 {
            self.anchor = to;
        }
        self.delete_selection();
        self.search();
    }

    /// X position where the search text starts (may be off to the left).
    fn box_text_x(&self) -> i32 {
        14 * self.u - self.box_scroll
    }

    /// Puts `text` on the clipboard; `file` says it is the path of a
    /// file being copied as a file.
    fn copy(&mut self, text: String, file: bool) {
        self.owns_clipboard = own_clipboard(&mut self.c, self.win, self.clipboard).unwrap_or(false);
        self.copied_file = file;
        self.status = if self.owns_clipboard && file {
            format!("Copied the file {text} (paste it in a file manager)")
        } else if self.owns_clipboard {
            format!("Copied {text}")
        } else {
            "Could not copy: the clipboard is not available".into()
        };
        self.copied = text;
    }

    /// On exit: if our copy is still the clipboard content, pass it to a
    /// small background process so it survives the window closing.
    fn hand_over_clipboard(&mut self) {
        if !self.owns_clipboard || self.copied.is_empty() {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let child = std::process::Command::new(exe)
            .args(["--hold-clipboard", if self.copied_file { "file" } else { "text" }])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = io::Write::write_all(&mut stdin, self.copied.as_bytes());
            }
        }
    }

    /// Reloads the index if its file changed on disk (the ffd daemon or
    /// `ff index` rewrote it), keeping the query and scroll position.
    fn reload_if_changed(&mut self) -> bool {
        // The preferred index can change while we run: the daemon's index
        // appears when its service starts and vanishes when it is removed.
        if self.follow_default {
            if let Some(preferred) = ff_core::default_db_path().filter(|p| *p != self.db) {
                self.db = preferred;
                self.db_stamp = None;
            }
        }
        let stamp = std::fs::metadata(&self.db).and_then(|m| m.modified()).ok();
        if stamp == self.db_stamp {
            return false;
        }
        let Ok(index) = Index::load(&self.db) else { return false }; // mid-rewrite: retry next tick
        self.db_stamp = stamp;
        self.index = index;
        let (sel, top) = (self.sel, self.top);
        self.search();
        self.sel = sel.min(self.hits.len().saturating_sub(1));
        self.top = top.min(self.max_top());
        true
    }

    fn reindex(&mut self) -> io::Result<()> {
        if self.db.as_os_str() == ff_core::SYSTEM_DB {
            self.status = "The index is kept up to date by the ffd daemon".into();
            return Ok(());
        }
        self.status = "Indexing...".into();
        self.render()?;
        match Index::build(&BuildOptions::refresh(&self.index)) {
            Ok(index) => {
                self.status = match index.save(&self.db) {
                    Ok(()) => String::new(),
                    Err(e) => format!("could not save the index: {e}"),
                };
                self.db_stamp = std::fs::metadata(&self.db).and_then(|m| m.modified()).ok();
                self.index = index;
            }
            Err(e) => self.status = format!("indexing failed: {e}"),
        }
        self.search();
        Ok(())
    }

    /// Runs a menu or keyboard action.
    fn act(&mut self, action: Action, time: u32) {
        // First the actions that do not need a selected result.
        let (a, b) = self.selection();
        match action {
            Action::Cut | Action::Copy if a != b => {
                self.copy(self.query[a..b].to_string(), false);
                if action == Action::Cut {
                    self.insert("");
                }
                return;
            }
            Action::Cut => return,
            // With no search text selected, Copy means the selected file.
            Action::Copy => return self.act(Action::CopyFile, time),
            Action::Paste => {
                // ConvertSelection: the answer arrives as SelectionNotify.
                let body = [W(self.win), W(self.clipboard), W(self.utf8_string), W(self.paste_prop), W(0)];
                self.c.req(24, 0, &body);
                return;
            }
            Action::SelectAll => return (self.anchor, self.cursor) = (0, self.query.len()),
            Action::Show(kind) => {
                self.kind = kind;
                return self.search();
            }
            Action::Case(case) => {
                self.case = case;
                return self.search();
            }
            Action::Reindex => {
                if let Err(e) = self.reindex() {
                    self.status = e.to_string();
                }
                return;
            }
            Action::About => return self.about = true,
            Action::Quit => return self.quit = true,
            Action::ZoomIn => return self.set_scale(Some(self.scale + 0.25)),
            Action::ZoomOut => return self.set_scale(Some(self.scale - 0.25)),
            Action::ZoomReset => return self.set_scale(None),
            Action::ToggleTitleBar => {
                self.custom_title = !self.custom_title;
                self.apply_title_bar();
                self.shown.clear();
                return self.save_config();
            }
            Action::ToggleShortcut => {
                self.status = match settings::set_shortcut(!settings::shortcut_enabled()) {
                    Ok(true) => "Alt+E now opens Fast search".into(),
                    Ok(false) => "Global shortcut removed".into(),
                    Err(e) => format!("could not change the shortcut: {e}"),
                };
                return;
            }
            Action::ToggleLauncher => {
                let done = if settings::launcher_installed() { settings::remove_launcher() } else { settings::install_launcher().map(drop) };
                self.status = match done {
                    Ok(()) if settings::launcher_installed() => "Added to the applications menu".into(),
                    Ok(()) => "Removed from the applications menu".into(),
                    Err(e) => format!("could not update the applications menu: {e}"),
                };
                return;
            }
            Action::ToggleDaemon => {
                self.status = "Waiting for authorization...".into();
                let (install, tx) = (!settings::daemon_installed(), self.notice_tx.clone());
                std::thread::spawn(move || tx.send(settings::set_daemon(install)));
                return;
            }
            _ => {}
        }
        let Some(&id) = self.hits.get(self.sel) else { return };
        let path = self.index.path(id);
        match action {
            Action::Open => {
                self.status = format!("Opening {}", path.display());
                std::thread::spawn(move || desktop::open(&path));
            }
            Action::Reveal => {
                self.status = format!("Showing {} in the file manager", path.display());
                std::thread::spawn(move || desktop::reveal(&path));
            }
            Action::Edit => {
                self.status = format!("Opening {} in the text editor", path.display());
                std::thread::spawn(move || desktop::open_in_editor(&path));
            }
            Action::CopyFile => self.copy(path.to_string_lossy().into_owned(), true),
            Action::CopyPath | Action::CopyName => {
                let text = match action {
                    Action::CopyName => String::from_utf8_lossy(self.index.name(id)).into_owned(),
                    _ => path.to_string_lossy().into_owned(),
                };
                self.copy(text, false);
            }
            Action::Properties => match desktop::properties(&path) {
                Ok(props) => self.props = Some(props),
                Err(e) => self.status = format!("{}: {e}", path.display()),
            },
            _ => {}
        }
    }

    /// The right-click menu for the selected result: text-like files
    /// (text, code, unknown types) also offer the text editor.
    fn context_items(&self) -> Items {
        let Some(&id) = self.hits.get(self.sel) else { return CONTEXT_MENU };
        let icon = Icon::of(self.index.name(id), self.index.is_dir(id), self.index.is_symlink(id));
        if matches!(icon, Icon::Text | Icon::Code | Icon::File | Icon::Link) { TEXT_CONTEXT_MENU } else { CONTEXT_MENU }
    }

    /// True if a toggle or choice in a menu is currently on.
    fn checked(&self, action: Action) -> bool {
        match action {
            Action::Show(kind) => kind == self.kind,
            Action::Case(case) => case == self.case,
            Action::ToggleTitleBar => self.custom_title,
            Action::ToggleShortcut => self.shortcut_on,
            Action::ToggleLauncher => self.launcher_on,
            Action::ToggleDaemon => self.daemon_on,
            _ => false,
        }
    }

    /// Tells the window manager whether to draw its title bar and frame.
    fn apply_title_bar(&mut self) {
        let decorations = if self.custom_title { 0u32 } else { 1 };
        let hints: Vec<u8> = [2, 0, decorations, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        self.c.set_property(self.win, self.motif_hints, self.motif_hints, 32, &hints);
    }

    fn save_config(&mut self) {
        // Only a zoom the user chose is saved; otherwise we keep following the desktop.
        let scale = (self.scale != self.auto_scale).then_some(self.scale);
        if let Err(e) = (Config { scale, own_title_bar: self.custom_title }).save() {
            self.status = format!("could not save settings: {e}");
        }
    }

    /// Changes the zoom (`None`: back to the desktop scale) and remembers it.
    fn set_scale(&mut self, scale: Option<f32>) {
        let scale = scale.unwrap_or(self.auto_scale).clamp(0.75, 4.0);
        let Ok(mut text) = Text::load((FONT_PX * scale).round()) else { return };
        self.scale = scale;
        self.u = (scale.round() as i32).max(1);
        self.size_w = text.width("000,000,000 KB") + 16 * self.u;
        self.date_w = text.width("0000-00-00 00:00") + 16 * self.u;
        self.text = text;
        self.icons = Icons::new((16 * self.u) as usize);
        self.about_icon = Icons::new((72 * self.u) as usize);
        self.menu = None;
        self.status = format!("Zoom {:.0}%", scale * 100.0);
        self.save_config();
    }

    /// Closes the properties or About panel; true if one was open.
    fn close_panel(&mut self) -> bool {
        self.props.take().is_some() | std::mem::take(&mut self.about)
    }

    // ---- our own title bar --------------------------------------------

    /// Sends a request to the window manager (a ClientMessage to the root).
    fn wm_message(&mut self, kind: u32, data: [u32; 5]) {
        let [a, b, c, d, e] = data;
        // SendEvent with SubstructureRedirect | SubstructureNotify.
        let event =
            [W(self.c.root), W(0x18_0000), B(EV_CLIENT_MESSAGE), B(32), H(0), W(self.win), W(kind), W(a), W(b), W(c), W(d), W(e)];
        self.c.req(25, 0, &event);
    }

    /// Hands the current mouse press to the window manager as a move
    /// (direction 8) or a resize from an edge or corner (0-7).
    fn begin_move_resize(&mut self, direction: u32, time: u32) {
        self.c.req(27, 0, &[W(time)]); // UngrabPointer, so the WM can grab it
        let (x, y) = self.pointer_root;
        self.wm_message(self.net_wm_moveresize, [x as u32, y as u32, direction, 1, 1]);
    }

    fn toggle_maximize(&mut self) {
        let [vert, horz] = self.net_wm_maximized;
        self.wm_message(self.net_wm_state, [2, vert, horz, 1, 0]); // 2 = toggle
    }

    fn title_button_at(&self, x: i32, y: i32) -> Option<usize> {
        let (w, bw) = (self.cv.w as i32, 44 * self.u);
        (self.custom_title && y >= 0 && y < self.title_h() && x >= w - 3 * bw && x < w).then(|| ((x - (w - 3 * bw)) / bw) as usize)
    }

    /// Resize direction if (x, y) is on the window's edge.
    fn resize_edge(&self, x: i32, y: i32) -> Option<u32> {
        let (w, h, m) = (self.cv.w as i32, self.cv.h as i32, 5 * self.u);
        let (left, right, top, bottom) = (x < m, x >= w - m, y < m, y >= h - m);
        // _NET_WM_MOVERESIZE: 0 top-left, then clockwise to 7 left.
        match (left, right, top, bottom) {
            (true, _, true, _) => Some(0),
            (_, true, true, _) => Some(2),
            (_, true, _, true) => Some(4),
            (true, _, _, true) => Some(6),
            (_, _, true, _) => Some(1),
            (_, true, _, _) => Some(3),
            (_, _, _, true) => Some(5),
            (true, _, _, _) => Some(7),
            _ => None,
        }
    }

    /// A left press on the title bar or window edge; true if handled.
    fn press_frame(&mut self, x: i32, y: i32, time: u32) -> bool {
        if !self.custom_title {
            return false;
        }
        if let Some(button) = self.title_button_at(x, y) {
            match button {
                0 => self.wm_message(self.wm_change_state, [3, 0, 0, 0, 0]), // IconicState
                1 => self.toggle_maximize(),
                _ => self.quit = true,
            }
        } else if let Some(direction) = self.resize_edge(x, y) {
            self.begin_move_resize(direction, time);
        } else if y < self.title_h() {
            if time.wrapping_sub(self.last_title_click) < 400 {
                self.toggle_maximize();
            } else {
                self.begin_move_resize(8, time);
            }
            self.last_title_click = time;
        } else {
            return false;
        }
        true
    }

    fn draw_title_bar(&mut self) {
        let (w, th, u) = (self.cv.w as i32, self.title_h(), self.u);
        self.cv.fill(0, 0, w, th, 0xe6e6e6);
        self.cv.fill(0, th - 1, w, 1, LINE);
        let icon = self.icons.size as i32;
        self.icons.draw(&mut self.cv, 10 * u, (th - icon) / 2, Icon::App);
        self.text.draw(&mut self.cv, 16 * u + icon, (th - self.text.height) / 2, "Fast search", w / 2, TEXT);
        let bw = 44 * u;
        for button in 0..3 {
            let x = w - (3 - button) * bw;
            let hovered = self.hover_button == Some(button as usize);
            let ink = if hovered && button == 2 { WHITE } else { 0x3a3a3a };
            if hovered {
                self.cv.fill(x, 0, bw, th - 1, if button == 2 { 0xd93025 } else { 0xd2d2d2 });
            }
            // 10-unit glyphs centred in the button.
            let (gx, gy, g, t) = (x + (bw - 10 * u) / 2, (th - 10 * u) / 2, 10 * u, u.max(1));
            match button {
                0 => self.cv.fill(gx, gy + g - 2 * t, g, t, ink),
                1 => self.cv.frame(gx, gy, g, g, t, ink),
                _ => {
                    for i in 0..g {
                        self.cv.fill(gx + i, gy + i, t, t, ink);
                        self.cv.fill(gx + g - 1 - i, gy + i, t, t, ink);
                    }
                }
            }
        }
    }

    /// (x, width) of each menu-bar title.
    fn bar_titles(&mut self) -> [(i32, i32); 5] {
        let mut x = 4 * self.u;
        BAR.map(|(title, _)| {
            let w = self.text.width(title) + 18 * self.u;
            x += w;
            (x - w, w)
        })
    }

    fn bar_title_at(&mut self, x: i32, y: i32) -> Option<usize> {
        let titles = self.bar_titles();
        titles.iter().position(|&(tx, tw)| y >= self.title_h() && y < self.menu_h() && x >= tx && x < tx + tw)
    }

    fn open_bar_menu(&mut self, i: usize) {
        if BAR[i].0 == "Options" {
            self.shortcut_on = settings::shortcut_enabled();
            self.launcher_on = settings::launcher_installed();
            self.daemon_on = settings::daemon_installed();
        }
        let (x, _) = self.bar_titles()[i];
        self.menu = Some(Menu { x, y: self.menu_h(), hover: None, items: BAR[i].1, bar: Some(i) });
    }

    fn rows_visible(&self) -> usize {
        ((self.cv.h as i32 - self.list_top() - self.row_h()) / self.row_h()).max(1) as usize
    }

    fn max_top(&self) -> usize {
        self.hits.len().saturating_sub(self.rows_visible())
    }

    fn move_sel(&mut self, delta: isize) {
        let last = self.hits.len().saturating_sub(1);
        self.sel = self.sel.saturating_add_signed(delta).min(last);
        let rows = self.rows_visible();
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + rows {
            self.top = self.sel + 1 - rows;
        }
    }

    /// Handles a key press; returns false to quit.
    fn key(&mut self, keycode: u8, state: u16, time: u32) -> io::Result<bool> {
        let key = self.c.key(keycode, state);
        if matches!(key, Key::None | Key::Sym(0xffe1..=0xffee)) {
            return Ok(true); // a modifier on its own
        }
        if self.close_panel() {
            return Ok(true); // any key closes a panel
        }
        if let Some(mut menu) = self.menu.take() {
            let n = menu.items.len();
            match key {
                Key::Sym(K_LEFT | K_RIGHT) if menu.bar.is_some() => {
                    let step = if matches!(key, Key::Sym(K_LEFT)) { BAR.len() - 1 } else { 1 };
                    self.open_bar_menu((menu.bar.unwrap() + step) % BAR.len());
                    self.menu.as_mut().unwrap().hover = Some(0);
                    return Ok(true);
                }
                Key::Sym(K_UP) => menu.hover = Some(menu.hover.map_or(n - 1, |h| (h + n - 1) % n)),
                Key::Sym(K_DOWN) => menu.hover = Some(menu.hover.map_or(0, |h| (h + 1) % n)),
                Key::Sym(K_RETURN | K_KP_ENTER) => {
                    if let Some(h) = menu.hover {
                        self.act(menu.items[h].2, time);
                    }
                    return Ok(true);
                }
                _ => return Ok(true), // anything else closes the menu
            }
            self.menu = Some(menu);
            return Ok(true);
        }

        let (ctrl, alt, shift) = (state & MOD_CTRL != 0, state & MOD_ALT != 0, state & MOD_SHIFT != 0);
        let page = self.rows_visible() as isize;
        let (sel_start, sel_end) = self.selection();
        self.status.clear();
        match key {
            Key::Char(ch) if ctrl => match ch.to_ascii_lowercase() {
                'q' => return Ok(false),
                '+' | '=' => self.act(Action::ZoomIn, time),
                '-' => self.act(Action::ZoomOut, time),
                '0' => self.act(Action::ZoomReset, time),
                'a' => self.act(Action::SelectAll, time),
                'c' if shift => self.act(Action::CopyPath, time),
                'c' => self.act(Action::Copy, time),
                'x' => self.act(Action::Cut, time),
                'v' => self.act(Action::Paste, time),
                'u' => self.clear_query(),
                'w' => self.delete_to(self.step(self.cursor, false, true)),
                _ => {}
            },
            Key::Char(ch) if !alt && !ch.is_control() => self.insert(ch.encode_utf8(&mut [0; 4])),
            Key::Sym(K_BACKSPACE) => self.delete_to(self.step(self.cursor, false, ctrl)),
            Key::Sym(K_DELETE) => self.delete_to(self.step(self.cursor, true, ctrl)),
            Key::Sym(K_LEFT | K_RIGHT) => {
                let right = matches!(key, Key::Sym(K_RIGHT));
                // Without Shift, a selection collapses to its near edge.
                let to = if !shift && !ctrl && sel_start != sel_end {
                    if right { sel_end } else { sel_start }
                } else {
                    self.step(self.cursor, right, ctrl)
                };
                self.move_caret(to, shift);
            }
            Key::Sym(K_HOME) if !ctrl => self.move_caret(0, shift),
            Key::Sym(K_END) if !ctrl => self.move_caret(self.query.len(), shift),
            Key::Sym(K_ESCAPE) => {
                if self.query.is_empty() {
                    return Ok(false);
                }
                self.clear_query();
            }
            Key::Sym(K_RETURN | K_KP_ENTER) => {
                let action = if alt {
                    Action::Properties
                } else if ctrl {
                    Action::Reveal
                } else {
                    Action::Open
                };
                self.act(action, time);
            }
            Key::Sym(K_MENU) if !self.hits.is_empty() => {
                let row = self.sel.saturating_sub(self.top) as i32;
                let y = self.list_top() + (row + 1) * self.row_h();
                self.menu = Some(Menu { x: 40 * self.u, y, hover: Some(0), items: self.context_items(), bar: None });
            }
            Key::Sym(K_UP) => self.move_sel(-1),
            Key::Sym(K_DOWN) => self.move_sel(1),
            Key::Sym(K_PAGE_UP) => self.move_sel(-page),
            Key::Sym(K_PAGE_DOWN) => self.move_sel(page),
            Key::Sym(K_HOME) => self.move_sel(isize::MIN),
            Key::Sym(K_END) => self.move_sel(isize::MAX),
            Key::Sym(K_F2) => {
                self.kind = match self.kind {
                    Kind::Any => Kind::File,
                    Kind::File => Kind::Dir,
                    Kind::Dir => Kind::Any,
                };
                self.search();
            }
            Key::Sym(K_F3) => {
                self.case = match self.case {
                    CaseMode::Smart => CaseMode::Sensitive,
                    CaseMode::Sensitive => CaseMode::Insensitive,
                    CaseMode::Insensitive => CaseMode::Smart,
                };
                self.search();
            }
            Key::Sym(K_F4) => self.act(Action::Edit, time),
            Key::Sym(K_F5) => self.reindex()?,
            _ => {}
        }
        Ok(true)
    }

    /// The result row under window position `y`, if any.
    fn row_at(&self, y: i32) -> Option<usize> {
        let row = self.top + ((y - self.list_top()) / self.row_h()) as usize;
        (y >= self.list_top() && row < self.hits.len() && row < self.top + self.rows_visible()).then_some(row)
    }

    fn press(&mut self, button: u8, x: i32, y: i32, time: u32, state: u16) {
        if self.close_panel() {
            return;
        }
        if let Some(menu) = self.menu.take() {
            if let (1, Some(item)) = (button, self.menu_item_at(&menu, x, y)) {
                self.act(menu.items[item].2, time);
            } else if let (1, Some(title)) = (button, self.bar_title_at(x, y)) {
                if menu.bar != Some(title) {
                    self.open_bar_menu(title); // another title: switch menus
                }
            }
            return; // any other click just closes the menu
        }
        if button == 1 && self.press_frame(x, y, time) {
            return;
        }
        let in_scrollbar = x >= self.cv.w as i32 - self.bar_w() && y >= self.list_top();
        match button {
            1 if y < self.menu_h() => {
                if let Some(title) = self.bar_title_at(x, y) {
                    self.open_bar_menu(title);
                }
            }
            1 if y < self.box_h() => {
                // Search box: place the caret; double-click selects a word.
                let query = self.query.clone();
                let at = self.text.index_at(&query, x - self.box_text_x());
                if time.wrapping_sub(self.last_box_click) < 400 {
                    self.anchor = self.step(self.step(at, true, false).min(query.len()), false, true);
                    self.cursor = self.step(self.anchor, true, true);
                } else {
                    self.move_caret(at, state & MOD_SHIFT != 0);
                    self.selecting = true;
                }
                self.last_box_click = time;
            }
            4 => self.top = self.top.saturating_sub(3),
            5 => self.top = (self.top + 3).min(self.max_top()),
            1 if in_scrollbar => {
                self.dragging_scrollbar = true;
                self.scroll_to(y);
            }
            1 | 3 if y >= self.list_top() => {
                let Some(row) = self.row_at(y) else { return };
                self.sel = row;
                self.status.clear();
                if button == 3 {
                    self.menu = Some(Menu { x, y, hover: None, items: self.context_items(), bar: None });
                    return;
                }
                let (last_time, last_row) = self.last_click;
                if last_row == row && time.wrapping_sub(last_time) < 400 {
                    // Double-click: the path column shows the folder.
                    let [_, (path_x, path_w), ..] = self.columns();
                    let on_path = x >= path_x && x < path_x + path_w;
                    self.act(if on_path { Action::Reveal } else { Action::Open }, time);
                }
                self.last_click = (time, row);
            }
            1 if y >= self.box_h() => {
                // Column header: sort by it, or flip the direction.
                let hit = self.columns().iter().rposition(|&(cx, _)| x >= cx).unwrap_or(0);
                let sort = [Sort::Name, Sort::Path, Sort::Size, Sort::Mtime][hit];
                self.reverse = self.sort == sort && !self.reverse;
                self.sort = sort;
                self.search();
            }
            _ => {}
        }
    }

    /// Pointer moved; returns true if something needs redrawing.
    fn motion(&mut self, x: i32, y: i32) -> bool {
        if self.dragging_scrollbar {
            let before = self.top;
            self.scroll_to(y);
            return self.top != before;
        }
        if self.selecting {
            let query = self.query.clone();
            let before = self.cursor;
            self.cursor = self.text.index_at(&query, x - self.box_text_x());
            return self.cursor != before;
        }
        if let Some(menu) = self.menu.take() {
            // With a bar menu open, pointing at another title switches to it.
            if let (Some(open), Some(title)) = (menu.bar, self.bar_title_at(x, y)) {
                if open != title {
                    self.open_bar_menu(title);
                    return true;
                }
            }
            let hover = self.menu_item_at(&menu, x, y);
            let changed = hover != menu.hover;
            self.menu = Some(Menu { hover, ..menu });
            return changed;
        }
        false
    }

    /// Scrolls so the scrollbar thumb is centred on window position `y`.
    fn scroll_to(&mut self, y: i32) {
        let (track, thumb) = self.scrollbar();
        let span = (track - thumb).max(1) as i64;
        let pos = ((y - self.list_top() - thumb / 2) as i64).clamp(0, span);
        self.top = (pos * self.max_top() as i64 / span) as usize;
    }

    // ---- layout --------------------------------------------------------

    fn row_h(&self) -> i32 {
        self.text.height + 6 * self.u
    }

    /// Height of our own title bar (0 when the desktop draws one).
    fn title_h(&self) -> i32 {
        if self.custom_title { self.row_h() + 10 * self.u } else { 0 }
    }

    /// Bottom edge of the menu bar.
    fn menu_h(&self) -> i32 {
        self.title_h() + self.row_h()
    }

    /// Bottom edge of the search box area (where the column headers start).
    fn box_h(&self) -> i32 {
        self.menu_h() + self.row_h() + 14 * self.u
    }

    fn list_top(&self) -> i32 {
        self.box_h() + self.row_h() + 1
    }

    fn bar_w(&self) -> i32 {
        10 * self.u
    }

    /// (x, width) in pixels of the Name, Path, Size and Date columns.
    fn columns(&self) -> [(i32, i32); 4] {
        let avail = (self.cv.w as i32 - self.bar_w() - self.size_w - self.date_w).max(0);
        let name = (avail * 2 / 5).max(120 * self.u);
        let mut x = 0;
        [name, (avail - name).max(0), self.size_w, self.date_w].map(|w| {
            let col = (x, w);
            x += w;
            col
        })
    }

    /// Scrollbar (track height, thumb height) in pixels.
    fn scrollbar(&self) -> (i32, i32) {
        let rows = self.rows_visible();
        let track = rows as i32 * self.row_h();
        let thumb = (track as i64 * rows as i64 / self.hits.len().max(1) as i64).max(24 * self.u as i64);
        (track, (thumb as i32).min(track))
    }

    /// Menu rectangle (x, y, w, h), kept inside the window.
    fn menu_rect(&mut self, menu: &Menu) -> (i32, i32, i32, i32) {
        let widest = menu.items.iter().map(|(l, s, _)| self.text.width(l) + self.text.width(s)).max().unwrap_or(0);
        let w = widest + 64 * self.u;
        let h = menu.items.len() as i32 * self.row_h() + 8 * self.u;
        let x = menu.x.min(self.cv.w as i32 - w).max(0);
        let y = menu.y.min(self.cv.h as i32 - h).max(0);
        (x, y, w, h)
    }

    fn menu_item_at(&mut self, menu: &Menu, px: i32, py: i32) -> Option<usize> {
        let (x, y, w, _) = self.menu_rect(menu);
        let item = (py - y - 4 * self.u).div_euclid(self.row_h());
        (px >= x && px < x + w && (0..menu.items.len() as i32).contains(&item)).then_some(item as usize)
    }

    // ---- drawing -------------------------------------------------------

    fn render(&mut self) -> io::Result<()> {
        let (w, h) = (self.cv.w as i32, self.cv.h as i32);
        let (u, row_h, bar) = (self.u, self.row_h(), self.bar_w());
        let pad = 8 * u;
        let cols = self.columns();
        self.cv.fill(0, 0, w, h, WHITE);

        // Search box: text scrolled to keep the caret visible, selection
        // highlighted, all clipped to the inside of the box.
        let top = self.menu_h();
        self.cv.fill(0, top, w, self.box_h() - top, PANEL);
        self.cv.fill(6 * u, top + 6 * u, w - 12 * u, row_h + 2 * u, WHITE);
        self.cv.frame(6 * u, top + 6 * u, w - 12 * u, row_h + 2 * u, u.max(1), 0x8fb4e8);

        // Title bar (ours) and menu bar.
        let th = self.title_h();
        if self.custom_title {
            self.draw_title_bar();
        }
        self.cv.fill(0, th, w, top - th, WHITE);
        self.cv.fill(0, top - 1, w, 1, LINE);
        let open = self.menu.as_ref().and_then(|m| m.bar);
        for (i, (x, tw)) in self.bar_titles().into_iter().enumerate() {
            if open == Some(i) {
                self.cv.fill(x, th, tw, top - 1 - th, HOVER);
            }
            self.text.draw(&mut self.cv, x + 9 * u, th + 3 * u, BAR[i].0, tw, TEXT);
        }
        let query = self.query.clone();
        let room = (w - 28 * u).max(4 * u);
        let caret = self.text.width(&query[..self.cursor]);
        let max_scroll = (self.text.width(&query) + 2 * u - room).max(0);
        self.box_scroll = self.box_scroll.clamp((caret + 2 * u - room).max(0), caret).min(max_scroll);
        let (tx, ty) = (self.box_text_x(), top + 10 * u);
        let box_clip = (14 * u, 14 * u + room);
        self.cv.clip = box_clip;
        self.text.draw(&mut self.cv, tx, ty, &query, i32::MAX / 2, TEXT);
        let (a, b) = self.selection();
        if a != b {
            let (xa, xb) = (tx + self.text.width(&query[..a]), tx + self.text.width(&query[..b]));
            self.cv.clip = (xa.max(box_clip.0), xb.min(box_clip.1));
            self.cv.fill(xa, ty - u, xb - xa, self.text.height + 2 * u, SELECT);
            self.text.draw(&mut self.cv, tx, ty, &query, i32::MAX / 2, WHITE);
            self.cv.clip = box_clip;
        }
        self.cv.fill(tx + caret, ty, u.max(1), self.text.height, TEXT);
        self.cv.clip = (0, i32::MAX);

        // Column headers with the sort direction.
        let y = self.box_h();
        self.cv.fill(0, y, w, row_h, PANEL);
        self.cv.fill(0, y + row_h, w, 1, LINE);
        let heads = [("Name", Sort::Name), ("Path", Sort::Path), ("Size", Sort::Size), ("Modified (UTC)", Sort::Mtime)];
        for (i, ((label, sort), (x, cw))) in heads.into_iter().zip(cols).enumerate() {
            let mark = if self.sort != sort { "" } else if self.reverse { "  ▼" } else { "  ▲" };
            if i > 0 {
                self.cv.fill(x, y + 4 * u, 1, row_h - 8 * u, LINE);
            }
            self.text.draw(&mut self.cv, x + pad, y + 3 * u, &format!("{label}{mark}"), cw - 2 * pad, TEXT);
        }

        // Result rows.
        let rows = self.rows_visible();
        let list_top = self.list_top();
        let icon = self.icons.size as i32;
        let mut path = Vec::new();
        for row in 0..rows {
            let Some(&id) = self.hits.get(self.top + row) else { break };
            let y = list_top + row as i32 * row_h;
            let selected = self.top + row == self.sel;
            let (fg, dim) = if selected { (WHITE, WHITE) } else { (TEXT, DIM) };
            if selected {
                self.cv.fill(0, y, w - bar, row_h, SELECT);
            }
            let is_dir = self.index.is_dir(id);
            let name = String::from_utf8_lossy(self.index.name(id)).into_owned();
            let parent = match self.index.parent(id) {
                Some(p) => {
                    self.index.write_path(p, &mut path);
                    String::from_utf8_lossy(&path).into_owned()
                }
                None => String::new(),
            };
            let kind = Icon::of(self.index.name(id), is_dir, self.index.is_symlink(id));
            self.icons.draw(&mut self.cv, cols[0].0 + pad, y + (row_h - icon) / 2, kind);
            let ty = y + 3 * u;
            let name_x = cols[0].0 + pad + icon + 6 * u;
            self.text.draw(&mut self.cv, name_x, ty, &name, cols[0].0 + cols[0].1 - pad - name_x, fg);
            self.text.draw(&mut self.cv, cols[1].0 + pad, ty, &parent, cols[1].1 - 2 * pad, dim);
            if !is_dir {
                let size = format!("{} KB", group_digits(self.index.size(id).div_ceil(1024)));
                let x = cols[2].0 + cols[2].1 - pad - self.text.width(&size);
                self.text.draw(&mut self.cv, x, ty, &size, cols[2].1, fg);
            }
            let (yr, mo, d, hh, mm) = ff_core::utc_civil(self.index.mtime(id));
            let date = format!("{yr:04}-{mo:02}-{d:02} {hh:02}:{mm:02}");
            self.text.draw(&mut self.cv, cols[3].0 + pad, ty, &date, cols[3].1 - pad, dim);
        }

        // Scrollbar.
        if self.hits.len() > rows {
            let (track, thumb) = self.scrollbar();
            let pos = ((track - thumb) as i64 * self.top as i64 / self.max_top().max(1) as i64) as i32;
            self.cv.fill(w - bar, list_top, bar, track, PANEL);
            self.cv.fill(w - bar + 2 * u, list_top + pos, bar - 4 * u, thumb, 0xa6a6a6);
        }

        // Status bar.
        let y = h - row_h;
        self.cv.fill(0, y, w, row_h, PANEL);
        self.cv.fill(0, y, w, 1, LINE);
        let status = if self.status.is_empty() {
            let kind = ["all", "files", "folders"][self.kind as usize];
            let case = ["smart", "sensitive", "insensitive"][self.case as usize];
            let n = group_digits(self.hits.len() as u64);
            format!("{n} objects      F2 show: {kind}      F3 case: {case}      F5 reindex      Right-click for actions")
        } else {
            self.status.clone()
        };
        self.text.draw(&mut self.cv, pad, y + 3 * u, &status, w - 2 * pad, TEXT);

        if let Some(menu) = self.menu.take() {
            self.draw_menu(&menu);
            self.menu = Some(menu);
        }
        if let Some(props) = self.props.take() {
            self.draw_properties(&props);
            self.props = Some(props);
        }
        if self.about {
            self.draw_about();
        }
        if self.custom_title {
            self.cv.frame(0, 0, w, h, 1, 0x8c8c8c); // the window's own border
        }
        self.present()
    }

    fn draw_menu(&mut self, menu: &Menu) {
        let (u, row_h) = (self.u, self.row_h());
        let (x, y, w, h) = self.menu_rect(menu);
        self.cv.fill(x + 3 * u, y + 3 * u, w, h, 0xbdbdbd); // shadow
        self.cv.fill(x, y, w, h, WHITE);
        self.cv.frame(x, y, w, h, u.max(1), 0x9a9a9a);
        for (i, (label, shortcut, action)) in menu.items.iter().enumerate() {
            let iy = y + 4 * u + i as i32 * row_h;
            if menu.hover == Some(i) {
                self.cv.fill(x + u, iy, w - 2 * u, row_h, HOVER);
            }
            // A bullet marks active choices and switched-on options.
            if self.checked(*action) {
                self.text.draw(&mut self.cv, x + 6 * u, iy + 3 * u, "\u{2022}", w, TEXT);
            }
            self.text.draw(&mut self.cv, x + 18 * u, iy + 3 * u, label, w, TEXT);
            let sx = x + w - 14 * u - self.text.width(shortcut);
            self.text.draw(&mut self.cv, sx, iy + 3 * u, shortcut, w, DIM);
        }
    }

    fn draw_properties(&mut self, props: &[(&'static str, String)]) {
        let (u, row_h) = (self.u, self.row_h());
        let (cw, ch) = (self.cv.w as i32, self.cv.h as i32);
        let w = (620 * u).min(cw - 20 * u);
        let h = (props.len() as i32 + 2) * row_h + 34 * u;
        let (x, y) = ((cw - w) / 2, ((ch - h) / 2).max(0));
        self.cv.fill(x + 4 * u, y + 4 * u, w, h, 0xb0b0b0); // shadow
        self.cv.fill(x, y, w, h, WHITE);
        self.cv.frame(x, y, w, h, u.max(1), 0x8a8a8a);
        self.cv.fill(x + u, y + u, w - 2 * u, row_h + 10 * u, PANEL);
        self.cv.fill(x + u, y + row_h + 11 * u, w - 2 * u, 1, LINE);
        self.text.draw(&mut self.cv, x + 16 * u, y + 8 * u, "Properties", w - 32 * u, TEXT);
        let label_w = 110 * u;
        for (i, (label, value)) in props.iter().enumerate() {
            let ry = y + row_h + 20 * u + i as i32 * row_h;
            self.text.draw(&mut self.cv, x + 16 * u, ry, label, label_w, DIM);
            self.text.draw(&mut self.cv, x + 16 * u + label_w, ry, value, w - 32 * u - label_w, TEXT);
        }
        let hint = "Press any key or click to close";
        self.text.draw(&mut self.cv, x + 16 * u, y + h - row_h - 4 * u, hint, w - 32 * u, DIM);
    }

    fn draw_about(&mut self) {
        let (u, row_h) = (self.u, self.row_h());
        let (cw, ch) = (self.cv.w as i32, self.cv.h as i32);
        let lines = [
            ("Fast search".to_string(), TEXT),
            (format!("Version {}", env!("CARGO_PKG_VERSION")), DIM),
            (String::new(), DIM),
            ("Instant file search for Linux".to_string(), TEXT),
            (format!("{} files and folders indexed", group_digits(self.index.len() as u64)), TEXT),
            (String::new(), DIM),
            ("github.com/khunder/fast-search".to_string(), SELECT),
            ("MIT License".to_string(), DIM),
            (String::new(), DIM),
            ("Press any key or click to close".to_string(), DIM),
        ];
        let icon = self.about_icon.size as i32;
        let w = (420 * u).min(cw - 20 * u);
        let h = icon + lines.len() as i32 * row_h + 44 * u;
        let (x, y) = ((cw - w) / 2, ((ch - h) / 2).max(0));
        self.cv.fill(x + 4 * u, y + 4 * u, w, h, 0xb0b0b0); // shadow
        self.cv.fill(x, y, w, h, WHITE);
        self.cv.frame(x, y, w, h, u.max(1), 0x8a8a8a);
        self.about_icon.draw(&mut self.cv, x + (w - icon) / 2, y + 16 * u, Icon::App);
        for (i, (line, color)) in lines.iter().enumerate() {
            let lx = x + (w - self.text.width(line)) / 2;
            self.text.draw(&mut self.cv, lx.max(x + 8 * u), y + icon + 26 * u + i as i32 * row_h, line, w - 16 * u, *color);
        }
    }

    /// Sends the rows that changed since the last frame to the window.
    fn present(&mut self) -> io::Result<()> {
        let stride = self.cv.w * 4;
        let same_size = self.shown.len() == self.cv.px.len();
        let mut row = 0;
        while row < self.cv.h {
            let changed = |r: usize| {
                !same_size || self.shown[r * stride..(r + 1) * stride] != self.cv.px[r * stride..(r + 1) * stride]
            };
            if !changed(row) {
                row += 1;
                continue;
            }
            let start = row;
            while row < self.cv.h && changed(row) {
                row += 1;
            }
            self.c.put_image(self.win, self.gc, self.cv.w, start, &self.cv.px[start * stride..row * stride])?;
        }
        self.shown.clone_from(&self.cv.px);
        self.c.flush()
    }
}

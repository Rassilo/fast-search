//! Folder sizes: what every folder holds in total, shown as a tree that
//! opens level by level and as a map where each tile's area is its size.
//! It all comes from the index, so nothing on disk is read.

use std::io;

use ff_core::{Index, group_digits};

use crate::backend::{Key, Mods};
use crate::canvas::{Icon, ON_ACCENT};
use crate::i18n::{tr, trf};
use crate::x11::*;
use crate::{Action, App, DIM, LINE, Menu, PANEL, SELECT, TEXT, WHITE};

/// The deepest nesting drawn inside the map.
const MAP_DEPTH: u8 = 3;
/// More tiles than this are too small to tell apart anyway.
const MAX_TILES: usize = 4000;

/// A visible line of the tree.
pub struct Node {
    pub id: u32,
    pub depth: u16,
    pub open: bool,
}

/// A rectangle of the map.
#[derive(Clone, Copy)]
pub struct Tile {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub depth: u8,
    /// Its contents are drawn inside it, under a title.
    pub framed: bool,
}

pub struct Tree {
    /// Per entry: a file's size, or everything inside a folder.
    total: Vec<u64>,
    /// Per entry: how many files it holds (1 for a file).
    files: Vec<u32>,
    /// The children of entry `i` are `kids[first[i]..first[i + 1]]`,
    /// largest first.
    first: Vec<u32>,
    kids: Vec<u32>,
    roots: Vec<u32>,
    pub rows: Vec<Node>,
    pub sel: usize,
    pub top: usize,
}

/// What to bring back after the index changed: the open folders and
/// the selected entry, as paths.
pub struct Kept {
    open: Vec<Vec<u8>>,
    sel: Option<Vec<u8>>,
    top: usize,
}

impl Tree {
    pub fn new(index: &Index) -> Tree {
        let n = index.len();
        let mut total = vec![0u64; n];
        let mut files = vec![0u32; n];
        let mut first = vec![0u32; n + 2];
        for id in 0..n as u32 {
            if let Some(p) = index.parent(id) {
                first[p as usize + 2] += 1;
            }
            if index.is_dir(id) {
                continue;
            }
            // A file counts for every folder above it.
            let size = index.size(id);
            (total[id as usize], files[id as usize]) = (size, 1);
            let mut up = index.parent(id);
            while let Some(p) = up {
                total[p as usize] += size;
                files[p as usize] += 1;
                up = index.parent(p);
            }
        }
        for i in 2..n + 2 {
            first[i] += first[i - 1];
        }
        let mut kids = vec![0u32; first[n + 1] as usize];
        let mut roots = Vec::new();
        for id in 0..n as u32 {
            match index.parent(id) {
                Some(p) => {
                    let slot = &mut first[p as usize + 1];
                    kids[*slot as usize] = id;
                    *slot += 1;
                }
                None => roots.push(id),
            }
        }
        first.pop();
        let largest_first = |list: &mut [u32]| list.sort_unstable_by(|&a, &b| total[b as usize].cmp(&total[a as usize]).then(a.cmp(&b)));
        for i in 0..n {
            largest_first(&mut kids[first[i] as usize..first[i + 1] as usize]);
        }
        largest_first(&mut roots);
        let rows = roots.iter().map(|&id| Node { id, depth: 0, open: false }).collect();
        let mut tree = Tree { total, files, first, kids, roots, rows, sel: 0, top: 0 };
        if tree.roots.len() == 1 {
            tree.open(0);
        }
        tree
    }

    pub fn total(&self, id: u32) -> u64 {
        self.total[id as usize]
    }

    pub fn files(&self, id: u32) -> u32 {
        self.files[id as usize]
    }

    pub fn kids(&self, id: u32) -> &[u32] {
        &self.kids[self.first[id as usize] as usize..self.first[id as usize + 1] as usize]
    }

    pub fn selected(&self) -> Option<u32> {
        self.rows.get(self.sel).map(|n| n.id)
    }

    /// Shows the children of a row; returns false if it has none.
    pub fn open(&mut self, row: usize) -> bool {
        let Some(node) = self.rows.get(row).filter(|n| !n.open) else { return false };
        let (id, depth) = (node.id, node.depth + 1);
        let range = self.first[id as usize] as usize..self.first[id as usize + 1] as usize;
        if range.is_empty() {
            return false;
        }
        let children: Vec<Node> = self.kids[range].iter().map(|&id| Node { id, depth, open: false }).collect();
        if self.sel > row {
            self.sel += children.len();
        }
        self.rows[row].open = true;
        self.rows.splice(row + 1..row + 1, children);
        true
    }

    pub fn close(&mut self, row: usize) {
        let Some(node) = self.rows.get_mut(row).filter(|n| n.open) else { return };
        node.open = false;
        let depth = node.depth;
        let end = self.rows[row + 1..].iter().position(|n| n.depth <= depth).map_or(self.rows.len(), |i| row + 1 + i);
        // A selection inside the closed folder moves to the folder.
        if self.sel >= end {
            self.sel -= end - row - 1;
        } else if self.sel > row {
            self.sel = row;
        }
        self.rows.drain(row + 1..end);
    }

    pub fn parent_row(&self, row: usize) -> Option<usize> {
        let depth = self.rows.get(row)?.depth;
        self.rows[..row].iter().rposition(|n| n.depth < depth)
    }

    /// Opens the folders leading to `id` and selects it.
    pub fn reveal(&mut self, index: &Index, id: u32) -> bool {
        let mut chain = vec![id];
        while let Some(p) = index.parent(*chain.last().unwrap()) {
            chain.push(p);
        }
        let root = chain.pop().unwrap();
        let Some(mut row) = self.rows.iter().position(|n| n.depth == 0 && n.id == root) else { return false };
        while let Some(next) = chain.pop() {
            self.open(row);
            let depth = self.rows[row].depth;
            let inside = self.rows[row + 1..].iter().take_while(|n| n.depth > depth);
            let Some(i) = inside.into_iter().position(|n| n.depth == depth + 1 && n.id == next) else { return false };
            row += 1 + i;
        }
        self.sel = row;
        true
    }

    /// The entry with that full path, if it is in the index.
    fn find(&self, index: &Index, path: &[u8]) -> Option<u32> {
        let inside = |root: &[u8]| path.strip_prefix(root).filter(|rest| rest.is_empty() || rest[0] == b'/' || root.ends_with(b"/"));
        let root = self.roots.iter().copied().filter(|&r| inside(index.name(r)).is_some()).max_by_key(|&r| index.name(r).len())?;
        let rest = inside(index.name(root))?;
        rest.split(|&b| b == b'/').filter(|part| !part.is_empty()).try_fold(root, |dir, part| self.kids(dir).iter().copied().find(|&k| index.name(k) == part))
    }

    pub fn keep(&self, index: &Index) -> Kept {
        let path = |id: u32| {
            let mut buf = Vec::new();
            index.write_path(id, &mut buf);
            buf
        };
        Kept { open: self.rows.iter().filter(|n| n.open).map(|n| path(n.id)).collect(), sel: self.selected().map(path), top: self.top }
    }

    /// The tree of a new index, opened like the old one was.
    pub fn restored(index: &Index, kept: &Kept) -> Tree {
        let mut tree = Tree::new(index);
        for path in &kept.open {
            if let Some(id) = tree.find(index, path) {
                if tree.reveal(index, id) {
                    tree.open(tree.sel);
                }
            }
        }
        tree.sel = 0;
        // What was selected may be gone: then the nearest folder still there.
        let mut path = kept.sel.as_deref();
        while let Some(p) = path {
            if tree.find(index, p).is_some_and(|id| tree.reveal(index, id)) {
                break;
            }
            path = p.iter().rposition(|&b| b == b'/').filter(|&i| i > 0).map(|i| &p[..i]);
        }
        tree.top = kept.top;
        tree
    }

    /// Lays the contents of folder `id` out in a rectangle, each tile's
    /// area in proportion to its size; large folders show their own
    /// contents under a title `header` pixels high.
    pub fn map(&self, id: u32, rect: (i32, i32, i32, i32), header: i32, out: &mut Vec<Tile>) {
        let (x, y, w, h) = rect;
        out.push(Tile { id, x, y, w, h, depth: 0, framed: true });
        self.lay(id, (x + 2, y + header, w - 4, h - header - 2), 1, header, out);
    }

    fn lay(&self, id: u32, rect: (i32, i32, i32, i32), depth: u8, header: i32, out: &mut Vec<Tile>) {
        let (x, y, w, h) = rect;
        let sum = self.total(id);
        if sum == 0 || w < 4 || h < 4 {
            return;
        }
        // Children are sorted, so the ones too small to draw are the tail.
        let scale = w as f64 * h as f64 / sum as f64;
        let room = MAX_TILES.saturating_sub(out.len());
        let areas: Vec<f64> = self.kids(id).iter().map(|&k| self.total(k) as f64 * scale).take_while(|&a| a >= 12.0).take(room).collect();
        let rects = squarify(&areas, x as f64, y as f64, w as f64, h as f64);
        for (&kid, (rx, ry, rw, rh)) in self.kids(id).iter().zip(rects) {
            // Rounding both edges keeps neighbours flush.
            let (x0, y0) = (rx.round() as i32, ry.round() as i32);
            let (tw, th) = ((rx + rw).round() as i32 - x0, (ry + rh).round() as i32 - y0);
            if tw < 1 || th < 1 {
                continue;
            }
            let framed = depth < MAP_DEPTH && !self.kids(kid).is_empty() && tw > 4 * header && th > 3 * header;
            out.push(Tile { id: kid, x: x0, y: y0, w: tw, h: th, depth, framed });
            if framed {
                self.lay(kid, (x0 + 2, y0 + header, tw - 4, th - header - 2), depth + 1, header, out);
            }
        }
    }
}

/// The "squarified" layout: fills the rectangle strip by strip along its
/// short side, closing a strip when one more tile would make its tiles
/// less square. `areas` are in square pixels, largest first.
fn squarify(areas: &[f64], mut x: f64, mut y: f64, mut w: f64, mut h: f64) -> Vec<(f64, f64, f64, f64)> {
    let mut out = Vec::with_capacity(areas.len());
    let mut i = 0;
    while i < areas.len() {
        let side = w.min(h);
        if side <= 0.0 {
            break;
        }
        // The worst width-to-height ratio in a strip of that total.
        let worst = |sum: f64, small: f64, large: f64| (side * side * large / (sum * sum)).max(sum * sum / (side * side * small));
        let (mut j, mut sum) = (i + 1, areas[i]);
        let mut ratio = worst(sum, areas[i], areas[i]);
        while j < areas.len() {
            let with = worst(sum + areas[j], areas[j], areas[i]);
            if with > ratio {
                break;
            }
            (sum, ratio, j) = (sum + areas[j], with, j + 1);
        }
        let thick = sum / side;
        let mut along = 0.0;
        for &area in &areas[i..j] {
            let len = area / thick;
            out.push(if w >= h { (x, y + along, thick, len) } else { (x + along, y, len, thick) });
            along += len;
        }
        if w >= h {
            (x, w) = (x + thick, w - thick);
        } else {
            (y, h) = (y + thick, h - thick);
        }
        i = j;
    }
    out
}

/// A size in the largest unit that keeps the number short. Totals span
/// bytes to terabytes, so the view does not use the results' fixed unit.
fn units(bytes: u64) -> String {
    let (mut value, mut unit) = (bytes as f64, "B");
    for next in ["KB", "MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" { format!("{bytes} B") } else { format!("{value:.1} {unit}") }
}

// ---- the view, inside the main window ----------------------------------

impl App {
    /// Opens the folder sizes view, or goes back to the search.
    pub(crate) fn toggle_tree(&mut self) {
        self.tiles.clear();
        self.tile_hover = None;
        if self.tree.take().is_some() {
            self.top = self.top.min(self.max_top());
            return;
        }
        let mut tree = Tree::new(&self.index);
        // Coming from a search, start at the selected result.
        if let Some(&id) = self.hits.get(self.sel).filter(|_| !self.query.trim().is_empty()) {
            if tree.reveal(&self.index, id) {
                tree.open(tree.sel);
            }
        }
        self.tree = Some(tree);
        self.show_tree_sel();
    }

    pub(crate) fn keep_tree(&self) -> Option<Kept> {
        self.tree.as_ref().map(|t| t.keep(&self.index))
    }

    /// Recomputes the view after the index changed.
    pub(crate) fn restore_tree(&mut self, kept: Option<Kept>) {
        if let Some(kept) = kept {
            self.tree = Some(Tree::restored(&self.index, &kept));
            let max = self.max_top();
            if let Some(tree) = self.tree.as_mut() {
                tree.top = tree.top.min(max);
            }
        }
    }

    /// Runs `f` as if the tree's selected entry were the only search result,
    /// so the actions on results work on it.
    pub(crate) fn on_tree_target<R>(&mut self, f: impl FnOnce(&mut App) -> R) -> Option<R> {
        let id = self.tree.as_ref()?.selected()?;
        let tree = self.tree.take();
        let saved = (std::mem::replace(&mut self.hits, vec![id]), self.sel, std::mem::take(&mut self.marks));
        self.sel = 0;
        let result = f(self);
        (self.hits, self.sel, self.marks) = saved;
        // The action may have changed the index and rebuilt nothing: the
        // callers that replace the index restore the tree themselves.
        if self.tree.is_none() {
            self.tree = tree;
        }
        Some(result)
    }

    /// Scrolls the tree so its selected row is visible.
    fn show_tree_sel(&mut self) {
        let rows = self.rows_visible();
        if let Some(tree) = self.tree.as_mut() {
            tree.sel = tree.sel.min(tree.rows.len().saturating_sub(1));
            if tree.sel < tree.top {
                tree.top = tree.sel;
            } else if tree.sel >= tree.top + rows {
                tree.top = tree.sel + 1 - rows;
            }
            tree.top = tree.top.min(tree.rows.len().saturating_sub(rows));
        }
    }

    /// Where the tree ends and the map starts (the window's width when
    /// it is too narrow for a map).
    fn tree_split(&self) -> i32 {
        let w = self.cv.w as i32;
        if w < 640 * self.u { w } else { (w * 46 / 100).max(380 * self.u) }
    }

    /// (x, width) of the Name, Size, Files and Share columns.
    fn tree_columns(&mut self) -> [(i32, i32); 4] {
        let pad = 8 * self.u;
        let files = self.text.width("9,999,999") + 2 * pad;
        let share = self.text.width("100.0 %").max(self.text.width(tr("Share"))) + 2 * pad;
        let size = self.text.width("1,023.9 MB") + 2 * pad;
        let name = (self.tree_split() - self.bar_w() - size - files - share).max(120 * self.u);
        let mut x = 0;
        [name, size, files, share].map(|w| {
            let col = (x, w);
            x += w;
            col
        })
    }

    fn tree_row_at(&self, x: i32, y: i32) -> Option<usize> {
        let tree = self.tree.as_ref()?;
        let row = tree.top + ((y - self.list_top()) / self.row_h()) as usize;
        (x < self.tree_split() - self.bar_w() && y >= self.list_top() && row < tree.rows.len() && row < tree.top + self.rows_visible()).then_some(row)
    }

    /// The innermost tile of the map at a window position.
    fn tile_at(&self, x: i32, y: i32) -> Option<Tile> {
        self.tiles.iter().rev().find(|t| x >= t.x && x < t.x + t.w && y >= t.y && y < t.y + t.h).copied()
    }

    /// The folder the map shows: the selected one, or the one holding
    /// the selected file.
    fn map_root(&self, tree: &Tree) -> Option<u32> {
        let id = tree.selected()?;
        if self.index.is_dir(id) { Some(id) } else { self.index.parent(id) }
    }

    fn select_in_tree(&mut self, id: u32) {
        if let Some(tree) = self.tree.as_mut() {
            tree.reveal(&self.index, id);
        }
        self.show_tree_sel();
    }

    /// A key while the view is open; `None` leaves it to the search window.
    pub(crate) fn tree_key(&mut self, key: Key, mods: Mods) -> Option<io::Result<bool>> {
        let page = self.rows_visible();
        let tree = self.tree.as_mut()?;
        let last = tree.rows.len().saturating_sub(1);
        match key {
            Key::Sym(K_ESCAPE | K_F6) => self.toggle_tree(),
            Key::Sym(K_UP) => tree.sel = tree.sel.saturating_sub(1),
            Key::Sym(K_DOWN) => tree.sel = (tree.sel + 1).min(last),
            Key::Sym(K_PAGE_UP) => tree.sel = tree.sel.saturating_sub(page),
            Key::Sym(K_PAGE_DOWN) => tree.sel = (tree.sel + page).min(last),
            Key::Sym(K_HOME) => tree.sel = 0,
            Key::Sym(K_END) => tree.sel = last,
            // Right opens a folder, then steps into it; Left does the reverse.
            Key::Sym(K_RIGHT) => {
                if !tree.open(tree.sel) && tree.rows.get(tree.sel).is_some_and(|n| n.open) {
                    tree.sel += 1;
                }
            }
            Key::Sym(K_LEFT) => {
                if tree.rows.get(tree.sel).is_some_and(|n| n.open) {
                    tree.close(tree.sel);
                } else if let Some(parent) = tree.parent_row(tree.sel) {
                    tree.sel = parent;
                }
            }
            Key::Sym(K_BACKSPACE) => tree.sel = tree.parent_row(tree.sel).unwrap_or(tree.sel),
            Key::Sym(K_RETURN | K_KP_ENTER) if mods.ctrl => self.act(Action::Reveal, 0),
            Key::Sym(K_RETURN | K_KP_ENTER) if mods.alt => self.act(Action::Properties, 0),
            Key::Sym(K_RETURN | K_KP_ENTER) => {
                let sel = tree.sel;
                if tree.rows.get(sel).is_some_and(|n| n.open) {
                    tree.close(sel);
                } else if !tree.open(sel) {
                    self.act(Action::Open, 0);
                }
            }
            Key::Sym(K_DELETE) => self.act(if mods.shift { Action::DeleteForever } else { Action::Trash }, 0),
            Key::Sym(K_MENU) => {
                let row = tree.sel.saturating_sub(tree.top) as i32;
                let (x, y) = (40 * self.u, self.list_top() + (row + 1) * self.row_h());
                let items = self.on_tree_target(|app| app.context_items())?;
                self.menu = Some(Menu { x, y, hover: Some(0), items, bar: None });
            }
            // Typing goes back to the search.
            Key::Char(ch) if !mods.ctrl && !mods.alt && !ch.is_control() => {
                self.toggle_tree();
                return None;
            }
            _ => return None,
        }
        self.status.clear();
        self.show_tree_sel();
        Some(Ok(true))
    }

    /// A button press below the menu bar while the view is open.
    pub(crate) fn tree_press(&mut self, button: u8, x: i32, y: i32, time: u32) {
        let (split, bar, list_top) = (self.tree_split(), self.bar_w(), self.list_top());
        let max_top = self.max_top();
        let Some(tree) = self.tree.as_mut() else { return };
        match button {
            4 => return tree.top = tree.top.saturating_sub(3),
            5 => return tree.top = (tree.top + 3).min(max_top),
            1 | 3 => {}
            _ => return,
        }
        if button == 1 && x >= split - bar && x < split && y >= list_top {
            self.dragging_scrollbar = true;
            return self.scroll_to(y);
        }
        self.status.clear();
        let double = |app: &mut App, id: u32| {
            let twice = app.last_click.1 == id as usize && time.wrapping_sub(app.last_click.0) < 400;
            app.last_click = (time, id as usize);
            twice && button == 1
        };
        if let Some(row) = self.tree_row_at(x, y) {
            let Some(tree) = self.tree.as_mut() else { return };
            let node = &tree.rows[row];
            let (id, open) = (node.id, node.open);
            // The arrow in front of a folder opens and closes it.
            let arrow_x = 4 * self.u + node.depth as i32 * 14 * self.u;
            let on_arrow = button == 1 && x >= arrow_x && x < arrow_x + 16 * self.u;
            tree.sel = row;
            if double(self, id) || on_arrow {
                let tree = self.tree.as_mut().unwrap();
                if open {
                    tree.close(row);
                } else if !tree.open(row) && !on_arrow {
                    self.act(Action::Open, time);
                }
            }
        } else if let Some(tile) = self.tile_at(x, y) {
            // A click in the map goes into that folder.
            double(self, tile.id);
            self.select_in_tree(tile.id);
            if let Some(tree) = self.tree.as_mut() {
                tree.open(tree.sel);
            }
        } else {
            return;
        }
        self.show_tree_sel();
        if button == 3 {
            if let Some(items) = self.on_tree_target(|app| app.context_items()) {
                self.menu = Some(Menu { x, y, hover: None, items, bar: None });
            }
        }
    }

    /// The pointer moved; true if the tile under it changed.
    pub(crate) fn tree_motion(&mut self, x: i32, y: i32) -> bool {
        let hover = self.tile_at(x, y).map(|t| t.id).filter(|_| self.menu.is_none());
        std::mem::replace(&mut self.tile_hover, hover) != hover
    }

    /// The status bar text while the view is open.
    pub(crate) fn tree_status(&self) -> String {
        let Some(tree) = &self.tree else { return String::new() };
        let Some(id) = self.tile_hover.or(tree.selected()) else { return tr("The index is empty").to_string() };
        let size = format!("{} ({})", units(tree.total(id)), trf("{} bytes", &[&group_digits(tree.total(id))]));
        let what = if self.index.is_dir(id) { format!("{size}      {}", trf("{} files", &[&group_digits(tree.files(id) as u64)])) } else { size };
        format!("{}      {what}      {}", self.index.path(id).display(), tr("Esc: back to the search"))
    }

    /// Draws the view below the menu bar: the tree on the left, the map
    /// of the selected folder on the right.
    pub(crate) fn draw_tree(&mut self) {
        let (w, h) = (self.cv.w as i32, self.cv.h as i32);
        let (u, row_h, bar) = (self.u, self.row_h(), self.bar_w());
        let pad = 8 * u;
        let Some(tree) = self.tree.take() else { return };
        let split = self.tree_split();
        let cols = self.tree_columns();

        // Where the search box was: the path of the selected entry.
        let top = self.menu_h();
        let path = tree.selected().map(|id| self.index.path(id).display().to_string()).unwrap_or_default();
        // It cannot be edited, so it does not look like the search box.
        self.cv.fill(6 * u, top + 6 * u, w - 12 * u, row_h + 2 * u, PANEL);
        self.cv.frame(6 * u, top + 6 * u, w - 12 * u, row_h + 2 * u, u.max(1), LINE);
        let path_x = self.box_text_x();
        self.text.draw(&mut self.cv, path_x, top + 10 * u, &path, w - 28 * u, DIM);

        let y = self.box_h();
        self.cv.fill(0, y, w, row_h, PANEL);
        self.cv.fill(0, y + row_h, w, 1, LINE);
        for (i, (label, (x, cw))) in ["Name", "Size", "Files", "Share"].into_iter().zip(cols).enumerate() {
            if i > 0 {
                self.cv.fill(x, y + 4 * u, 1, row_h - 8 * u, LINE);
            }
            self.text.draw(&mut self.cv, x + pad, y + 3 * u, tr(label), cw - 2 * pad, TEXT);
        }

        // The tree.
        let rows = self.rows_visible();
        let list_top = self.list_top();
        let icon = self.icons.size as i32;
        let right = split - bar;
        self.cv.clip = (0, right);
        for row in 0..rows {
            let Some(node) = tree.rows.get(tree.top + row) else { break };
            let (id, y) = (node.id, list_top + row as i32 * row_h);
            let selected = tree.top + row == tree.sel;
            let (fg, dim) = if selected { (ON_ACCENT, ON_ACCENT) } else { (TEXT, DIM) };
            // Its share of the folder it is in, also drawn as a bar.
            let of = self.index.parent(id).map_or(tree.total(id), |p| tree.total(p));
            let share = if of == 0 { 0.0 } else { tree.total(id) as f64 / of as f64 };
            let x = pad / 2 + node.depth as i32 * 14 * u;
            if selected {
                self.cv.fill(0, y, right, row_h, SELECT);
            } else {
                let room = (cols[0].1 - x - 16 * u).max(0);
                self.cv.fill(x + 16 * u, y + 2 * u, (room as f64 * share) as i32, row_h - 4 * u, 0xfdebc4);
            }
            let is_dir = self.index.is_dir(id);
            self.cv.clip = (0, cols[0].1);
            if !tree.kids(id).is_empty() {
                self.draw_arrow(x + 5 * u, y + row_h / 2, node.open, dim);
            }
            let kind = Icon::of(self.index.name(id), is_dir, self.index.is_symlink(id));
            self.icons.draw(&mut self.cv, x + 16 * u, y + (row_h - icon) / 2, kind);
            let name = String::from_utf8_lossy(self.index.name(id)).into_owned();
            let (name_x, ty) = (x + 16 * u + icon + 6 * u, y + 3 * u);
            self.text.draw(&mut self.cv, name_x, ty, &name, cols[0].1 - pad - name_x, fg);
            self.cv.clip = (0, right);
            let files = if is_dir { group_digits(tree.files(id) as u64) } else { String::new() };
            let cells = [(units(tree.total(id)), fg), (files, dim), (format!("{:.1} %", share * 100.0), dim)];
            for ((cell, color), (cx, cw)) in cells.into_iter().zip(&cols[1..]) {
                let x = cx + cw - pad - self.text.width(&cell);
                self.text.draw(&mut self.cv, x, ty, &cell, *cw, color);
            }
        }
        self.cv.clip = (0, i32::MAX);
        if tree.rows.len() > rows {
            let (track, thumb) = self.scrollbar();
            let pos = ((track - thumb) as i64 * tree.top as i64 / (tree.rows.len() - rows).max(1) as i64) as i32;
            self.cv.fill(right, list_top, bar, track, PANEL);
            self.cv.fill(right + 2 * u, list_top + pos, bar - 4 * u, thumb, 0xa6a6a6);
        }

        // The map.
        self.tiles.clear();
        let (mx, my) = (split + 1, self.box_h());
        let (mw, mh) = (w - mx, h - row_h - my);
        if mw > 0 {
            self.cv.fill(split, my, 1, mh, LINE);
            self.cv.fill(mx, my, mw, mh, WHITE);
        }
        let header = self.text.height + 4 * u;
        if let Some(root) = self.map_root(&tree).filter(|_| mw > 8 * header && mh > 4 * header) {
            let mut tiles = std::mem::take(&mut self.tiles);
            tree.map(root, (mx + 4 * u, my + 4 * u, mw - 8 * u, mh - 8 * u), header, &mut tiles);
            for tile in &tiles {
                self.draw_tile(&tree, tile);
            }
            // Outlines last, so nothing drawn later covers them.
            for tile in &tiles {
                if Some(tile.id) == tree.selected() && tile.depth > 0 {
                    self.cv.frame(tile.x, tile.y, tile.w, tile.h, 2 * u, SELECT);
                } else if Some(tile.id) == self.tile_hover {
                    self.cv.frame(tile.x, tile.y, tile.w, tile.h, u.max(1), 0x333333);
                }
            }
            if tiles.len() == 1 {
                let note = tr(if tree.kids(root).is_empty() { "This folder is empty" } else { "Everything in this folder is empty" });
                self.text.draw(&mut self.cv, mx + 12 * u, my + 4 * u + header + 8 * u, note, mw - 24 * u, DIM);
            }
            self.tiles = tiles;
        }
        self.cv.clip = (0, i32::MAX);
        self.tree = Some(tree);
    }

    fn draw_tile(&mut self, tree: &Tree, tile: &Tile) {
        let u = self.u;
        let Tile { id, x, y, w, h, .. } = *tile;
        // Folders get deeper blues level by level; files are sand.
        let color = if self.index.is_dir(id) { [0xdce9fb, 0xcfe0f9, 0xc6d9f6, 0xc1d4f2][tile.depth.min(3) as usize] } else { 0xfbefd0 };
        self.cv.clip = (x, x + w);
        self.cv.fill(x, y, w, h, color);
        self.cv.frame(x, y, w, h, 1, WHITE);
        let name = String::from_utf8_lossy(self.index.name(id)).into_owned();
        let size = units(tree.total(id));
        self.cv.clip = (x, x + w - 2 * u);
        if tile.framed {
            self.text.draw(&mut self.cv, x + 4 * u, y + 2 * u, &format!("{name}  ({size})"), w - 6 * u, TEXT);
        } else if w > 44 * u && h >= self.text.height + 2 * u {
            self.text.draw(&mut self.cv, x + 4 * u, y + u, &name, w - 6 * u, TEXT);
            if h >= 2 * self.text.height + 3 * u {
                self.text.draw(&mut self.cv, x + 4 * u, y + u + self.text.height, &size, w - 6 * u, DIM);
            }
        }
    }

    /// A small triangle: pointing down for an open folder, right for a
    /// closed one. (`x`, `y`) is its centre.
    fn draw_arrow(&mut self, x: i32, y: i32, open: bool, color: u32) {
        let s = 4 * self.u;
        for i in 0..s {
            if open {
                self.cv.fill(x - s + i, y - s / 2 + i, 2 * (s - i), 1, color);
            } else {
                self.cv.fill(x - s / 2 + i, y - s + i, 1, 2 * (s - i), color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_tree_and_map() {
        let root = std::env::temp_dir().join(format!("ff_tree_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("big/deep")).unwrap();
        std::fs::create_dir_all(root.join("small")).unwrap();
        std::fs::write(root.join("big/deep/a.bin"), vec![0; 30_000]).unwrap();
        std::fs::write(root.join("big/b.bin"), vec![0; 10_000]).unwrap();
        std::fs::write(root.join("small/c.bin"), vec![0; 5_000]).unwrap();
        std::fs::write(root.join("d.bin"), vec![0; 20_000]).unwrap();
        let index = Index::build(&ff_core::BuildOptions::new([&root])).unwrap();
        let name = |id: u32| String::from_utf8_lossy(index.name(id)).into_owned();

        // One root: it starts open, its children largest first.
        let mut tree = Tree::new(&index);
        let top = tree.rows[0].id;
        assert_eq!((tree.total(top), tree.files(top)), (65_000, 4));
        let shown: Vec<_> = tree.rows[1..].iter().map(|n| (name(n.id), tree.total(n.id))).collect();
        assert_eq!(shown, [("big".to_string(), 40_000), ("d.bin".to_string(), 20_000), ("small".to_string(), 5_000)]);

        // Opening, closing and jumping to an entry.
        assert!(tree.open(1));
        assert_eq!(name(tree.rows[2].id), "deep");
        assert_eq!(tree.parent_row(2), Some(1));
        tree.sel = 2;
        tree.close(1);
        assert_eq!((tree.rows.len(), tree.sel), (4, 1));
        let deepest = (0..index.len() as u32).find(|&id| name(id) == "a.bin").unwrap();
        assert!(tree.reveal(&index, deepest));
        assert_eq!(tree.selected(), Some(deepest));
        assert_eq!(tree.rows[tree.sel].depth, 3);

        // The same folders are open after the index is replaced.
        let again = Tree::restored(&index, &tree.keep(&index));
        assert_eq!(again.selected(), Some(deepest));
        assert_eq!(again.rows.len(), tree.rows.len());

        // The map: tiles stay inside their folder and areas follow sizes.
        let mut tiles = Vec::new();
        tree.map(top, (0, 0, 400, 300), 20, &mut tiles);
        let tile = |n: &str| *tiles.iter().find(|t| name(t.id) == n).unwrap();
        let (big, d, a) = (tile("big"), tile("d.bin"), tile("a.bin"));
        assert!(big.framed && a.x >= big.x && a.y >= big.y + 20 && a.x + a.w <= big.x + big.w && a.y + a.h <= big.y + big.h);
        let ratio = (big.w * big.h) as f64 / (d.w * d.h) as f64;
        assert!((ratio - 2.0).abs() < 0.1, "{ratio}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn squarified_tiles_fill_the_rectangle() {
        let areas = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0].map(|a| a * 100.0);
        let rects = squarify(&areas, 0.0, 0.0, 60.0, 40.0);
        assert_eq!(rects.len(), areas.len());
        for (&area, &(x, y, w, h)) in areas.iter().zip(&rects) {
            assert!((w * h - area).abs() < 1e-6);
            assert!(x >= -1e-6 && y >= -1e-6 && x + w <= 60.0 + 1e-6 && y + h <= 40.0 + 1e-6);
            // The point of the algorithm: no long thin slivers.
            assert!(w.max(h) / w.min(h) < 4.0, "{w} x {h}");
        }
    }
}

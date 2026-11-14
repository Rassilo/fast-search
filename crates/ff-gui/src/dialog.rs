//! A reusable dialog window: a titled, modal window over the main one,
//! made of content rows (text, label/value pairs, checkboxes, buttons)
//! and a button bar. It comes in two shapes: a plain one sized to its
//! content (confirmations, help sheets), and one with a list of pages down
//! the left side (the Options window).

use super::*;

/// One line of dialog content.
pub enum Row {
    Text(String),
    /// Secondary text, also used for section headings.
    Dim(String),
    /// A label and its value.
    Pair(&'static str, String),
    /// A checkbox; ticking it runs the action, and its state is whatever
    /// `App::checked` reports for that action.
    Check(&'static str, Action),
    /// A button inside the content; pressing it runs the action and
    /// leaves the dialog open.
    Button(&'static str, Action),
    /// An entry of a list box (consecutive ones form the box); choosing
    /// it runs the action, and the chosen one is whichever `App::checked`
    /// reports for its action.
    ListItem(String, Action),
    /// A one-line text field (one per dialog; its text is
    /// `App::dialog_input`), with a hint shown while it is empty. Enter
    /// in the field runs the action.
    Input(&'static str, Action),
    /// Half a line of space.
    Gap,
}

/// A button of the bar at the bottom: its label and what it does (`None`
/// just closes the dialog). Pressing one closes the dialog.
pub type Button = (&'static str, Option<Action>);

/// An entry of the page list: a page, or a group heading that opens its
/// first page.
pub struct NavItem {
    pub label: &'static str,
    /// Pages are indented under their group.
    pub child: bool,
    pub action: Action,
}

/// Something in the dialog the user can activate.
#[derive(Clone, Copy, PartialEq)]
enum Item {
    Nav(usize),
    Check(Action),
    RowButton(Action),
    Input(Action),
    Button(usize),
}

type Rect = (i32, i32, i32, i32);

pub struct Dialog {
    pub win: u32,
    pub cv: Canvas,
    rows: Vec<Row>,
    buttons: Vec<Button>,
    /// The last button is the main one; draw it as dangerous (red).
    danger: bool,
    /// A reference sheet: pair labels are examples, shown prominently.
    pub sheet: bool,
    /// The page list; empty for a plain dialog.
    nav: Vec<NavItem>,
    nav_selected: usize,
    /// Title of the page shown beside the list.
    heading: String,
    /// Index into the activatable items.
    focus: usize,
    hover: Option<usize>,
}

/// Where everything in a dialog goes.
struct Layout {
    w: i32,
    h: i32,
    /// Left edge and width of the content rows.
    content: (i32, i32),
    /// Top of the first content row.
    top: i32,
    /// The page list box and the page panel, when there is a page list.
    panes: Option<(Rect, Rect)>,
    items: Vec<(i32, i32, i32, i32, Item)>,
}

impl App {
    /// Opens a plain dialog. `focus_button` is the button Enter triggers at first.
    pub fn open_dialog(
        &mut self,
        title: &str,
        rows: Vec<Row>,
        buttons: Vec<Button>,
        focus_button: usize,
        danger: bool,
    ) -> io::Result<()> {
        self.open_dialog_with(title, rows, buttons, focus_button, danger, Vec::new(), 0, "")
    }

    /// Opens a dialog with a page list down its left side, or, if such a
    /// dialog is already open, shows another page in it.
    pub fn open_paged_dialog(
        &mut self,
        title: &str,
        nav: Vec<NavItem>,
        nav_selected: usize,
        heading: &str,
        rows: Vec<Row>,
    ) -> io::Result<()> {
        if let Some(dialog) = self.dialog.as_mut().filter(|d| !d.nav.is_empty()) {
            // Same window, new page.
            // Redrawing the same page (a value changed) keeps the focus.
            if dialog.nav_selected != nav_selected {
                dialog.focus = nav_selected;
                dialog.hover = None;
            }
            dialog.rows = rows;
            dialog.nav_selected = nav_selected;
            dialog.heading = heading.to_string();
            return self.render_dialog();
        }
        // OK saves and closes, Cancel closes, Apply saves and stays.
        let buttons = vec![("OK", Some(Action::OptionsOk)), ("Cancel", None), ("Apply", Some(Action::OptionsApply))];
        self.open_dialog_with(title, rows, buttons, 0, false, nav, nav_selected, heading)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_dialog_with(
        &mut self,
        title: &str,
        rows: Vec<Row>,
        buttons: Vec<Button>,
        focus_button: usize,
        danger: bool,
        nav: Vec<NavItem>,
        nav_selected: usize,
        heading: &str,
    ) -> io::Result<()> {
        self.close_dialog();
        self.menu = None;
        let mut dialog = Dialog {
            win: self.c.new_id(),
            cv: Canvas::new(),
            rows,
            buttons,
            danger,
            sheet: false,
            nav,
            nav_selected,
            heading: heading.to_string(),
            focus: 0,
            hover: None,
        };
        let layout = self.dialog_layout(&dialog);
        let (w, h) = (layout.w, layout.h);
        dialog.focus = layout.items.iter().position(|i| i.4 == Item::Button(focus_button)).unwrap_or(0);
        dialog.cv.resize(w as usize, h as usize);

        // Events: KeyPress | ButtonPress | PointerMotion | Exposure.
        let (win, events) = (dialog.win, 0x1 | 0x4 | 0x40 | 0x8000);
        let c = &mut self.c;
        c.req(1, 0, &[W(win), W(c.root), H(0), H(0), H(w as u16), H(h as u16), H(0), H(1), W(0), W(0x800), W(events)]);
        let atom = |c: &mut Conn, name: &str| c.intern_atom(name);
        let words = |values: &[u32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        let net_wm_name = atom(c, "_NET_WM_NAME")?;
        let wm_protocols = atom(c, "WM_PROTOCOLS")?;
        let window_type = atom(c, "_NET_WM_WINDOW_TYPE")?;
        let type_dialog = atom(c, "_NET_WM_WINDOW_TYPE_DIALOG")?;
        let state_modal = atom(c, "_NET_WM_STATE_MODAL")?;
        c.set_property(win, ATOM_WM_NAME, ATOM_STRING, 8, title.as_bytes());
        c.set_property(win, net_wm_name, self.utf8_string, 8, title.as_bytes());
        c.set_property(win, ATOM_WM_CLASS, ATOM_STRING, 8, b"ff-gui\0ff-gui\0");
        c.set_property(win, wm_protocols, ATOM_ATOM, 32, &words(&[self.wm_delete]));
        c.set_property(win, 68, 33, 32, &words(&[self.win])); // WM_TRANSIENT_FOR: belongs to the main window
        c.set_property(win, window_type, ATOM_ATOM, 32, &words(&[type_dialog]));
        c.set_property(win, self.net_wm_state, ATOM_ATOM, 32, &words(&[state_modal]));
        // WM_NORMAL_HINTS: minimum size = maximum size, so it cannot be resized.
        let mut hints = [0u32; 18];
        hints[0] = 0x10 | 0x20;
        (hints[5], hints[6], hints[7], hints[8]) = (w as u32, h as u32, w as u32, h as u32);
        c.set_property(win, 40, 41, 32, &words(&hints));
        c.req(8, 0, &[W(win)]); // MapWindow

        self.dialog = Some(dialog);
        self.render_dialog()
    }

    pub fn close_dialog(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            self.c.req(4, 0, &[W(dialog.win)]); // DestroyWindow
            // The next time the Options window opens, its pages start fresh.
            self.options_page = None;
            self.index_settings_loaded = false;
            self.dialog_input.clear();
        }
    }

    /// True while the dialog with a page list is open.
    pub fn paged_dialog_open(&self) -> bool {
        self.dialog.as_ref().is_some_and(|d| !d.nav.is_empty())
    }

    /// Width of the label column of `Row::Pair` rows: fits the widest label.
    fn dialog_label_w(&mut self, d: &Dialog) -> i32 {
        let widest = d.rows.iter().map(|r| if let Row::Pair(label, _) = r { self.text.width(label) } else { 0 }).max();
        (widest.unwrap_or(0) + 22 * self.u).max(110 * self.u)
    }

    fn dialog_layout(&mut self, d: &Dialog) -> Layout {
        let (u, row_h) = (self.u, self.row_h());
        let pad = 18 * u;
        let paged = !d.nav.is_empty();
        let button_h = row_h + 8 * u;
        let button_w: Vec<i32> = d.buttons.iter().map(|(label, _)| self.text.width(label) + 36 * u).collect();
        let buttons_total = button_w.iter().sum::<i32>() + 10 * u * (button_w.len() as i32 - 1).max(0);
        let mut items = Vec::new();

        // A paged dialog has one fixed size, so pages can change in place;
        // a plain one fits its content.
        let (w, h, content, top, panes) = if paged {
            let (w, h) = (860 * u, 620 * u);
            let bottom = h - pad - button_h - 14 * u;
            let list = (pad, pad, 190 * u, bottom - pad);
            let panel = (pad + 200 * u, pad, w - 2 * pad - 200 * u, bottom - pad);
            let nav_h = self.text.height + 6 * u;
            for (i, item) in d.nav.iter().enumerate() {
                let indent = if item.child { 18 * u } else { 4 * u };
                let iw = self.text.width(item.label) + 10 * u;
                items.push((list.0 + indent, list.1 + 6 * u + i as i32 * nav_h, iw, nav_h, Item::Nav(i)));
            }
            let content = (panel.0 + 16 * u, panel.2 - 32 * u);
            (w, h, content, panel.1 + row_h + 22 * u, Some((list, panel)))
        } else {
            let label_w = self.dialog_label_w(d);
            let widest = d
                .rows
                .iter()
                .map(|row| match row {
                    Row::Text(t) | Row::Dim(t) => self.text.width(t),
                    Row::Pair(_, value) => label_w + self.text.width(value),
                    Row::Check(label, _) => 28 * u + self.text.width(label),
                    Row::Button(label, _) => 30 * u + self.text.width(label),
                    Row::ListItem(label, _) => 20 * u + self.text.width(label),
                    Row::Input(..) => 420 * u,
                    Row::Gap => 0,
                })
                .max()
                .unwrap_or(0);
            let w = (widest.max(buttons_total) + 2 * pad).clamp(380 * u, 980 * u);
            (w, 0, (pad, w - 2 * pad), pad, None)
        };

        let mut y = top;
        for row in &d.rows {
            match row {
                Row::Check(_, action) => items.push((content.0 - 4 * u, y, content.1 + 8 * u, row_h, Item::Check(*action))),
                Row::ListItem(_, action) => items.push((content.0, y, content.1, row_h, Item::Check(*action))),
                Row::Input(_, action) => items.push((content.0, y, content.1, row_h, Item::Input(*action))),
                Row::Button(label, action) => {
                    let bw = self.text.width(label) + 28 * u;
                    items.push((content.0, y + u, bw, row_h - 2 * u, Item::RowButton(*action)));
                }
                _ => {}
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }
        let h = if paged { h } else { y + 14 * u + button_h + pad };
        let by = h - pad - button_h;
        let mut x = w - pad - buttons_total;
        for (i, bw) in button_w.into_iter().enumerate() {
            items.push((x, by, bw, button_h, Item::Button(i)));
            x += bw + 10 * u;
        }
        Layout { w, h, content, top, panes, items }
    }

    pub fn render_dialog(&mut self) -> io::Result<()> {
        let Some(mut d) = self.dialog.take() else { return Ok(()) };
        let (u, row_h) = (self.u, self.row_h());
        let layout = self.dialog_layout(&d);
        let (w, h) = (layout.w, layout.h);
        let label_w = self.dialog_label_w(&d);
        let line = u.max(1);
        let item_at = |item: Item| layout.items.iter().position(|i| i.4 == item);
        d.cv.fill(0, 0, w, h, if layout.panes.is_some() { PANEL } else { WHITE });

        if let Some((list, panel)) = layout.panes {
            // The page list: a white box, the open page highlighted.
            d.cv.fill(list.0, list.1, list.2, list.3, WHITE);
            d.cv.frame(list.0, list.1, list.2, list.3, line, 0x9a9a9a);
            for (index, &(x, y, iw, ih, item)) in layout.items.iter().enumerate() {
                let Item::Nav(i) = item else { continue };
                let selected = i == d.nav_selected;
                if selected {
                    d.cv.fill(x - 2 * u, y, iw + 2 * u, ih, SELECT);
                } else if d.hover == Some(index) {
                    d.cv.fill(x - 2 * u, y, iw + 2 * u, ih, HOVER);
                }
                if d.nav[i].child {
                    // Tree lines: a trunk running down past every page of
                    // the group (stopping at the last one) and a branch
                    // out to each page.
                    let last_of_group = d.nav.get(i + 1).is_none_or(|next| !next.child);
                    let trunk = if last_of_group { ih / 2 + line } else { ih };
                    d.cv.fill(x - 10 * u, y, line, trunk, 0xa0a0a0);
                    d.cv.fill(x - 10 * u, y + ih / 2, 7 * u, line, 0xa0a0a0);
                }
                self.text.draw(&mut d.cv, x + 3 * u, y + 3 * u, d.nav[i].label, iw, if selected { ON_ACCENT } else { TEXT });
                if d.focus == index && !selected {
                    d.cv.frame(x - 2 * u, y, iw + 2 * u, ih, line, 0x5b9bea);
                }
            }
            // The page: a tab with its title on a bordered panel.
            let tab_w = self.text.width(&d.heading) + 24 * u;
            d.cv.fill(panel.0, panel.1 + row_h + 6 * u, panel.2, panel.3 - row_h - 6 * u, 0xf9f9f9);
            d.cv.frame(panel.0, panel.1 + row_h + 6 * u, panel.2, panel.3 - row_h - 6 * u, line, LINE);
            d.cv.fill(panel.0, panel.1, tab_w, row_h + 6 * u + line, 0xf9f9f9);
            d.cv.fill(panel.0, panel.1, tab_w, line, LINE);
            d.cv.fill(panel.0, panel.1, line, row_h + 6 * u, LINE);
            d.cv.fill(panel.0 + tab_w - line, panel.1, line, row_h + 6 * u, LINE);
            self.text.draw(&mut d.cv, panel.0 + 12 * u, panel.1 + 6 * u, &d.heading, tab_w, TEXT);
        }

        let (cx, cw) = layout.content;
        let mut y = layout.top;
        for row in &d.rows {
            let ty = y + 3 * u;
            match row {
                Row::Text(t) => self.text.draw(&mut d.cv, cx, ty, t, cw, TEXT),
                Row::Dim(t) => self.text.draw(&mut d.cv, cx, ty, t, cw, DIM),
                Row::Pair(label, value) => {
                    // On sheets of examples the label is what you type: dark, value dim.
                    let (label_ink, value_ink) = if d.sheet { (TEXT, DIM) } else { (DIM, TEXT) };
                    self.text.draw(&mut d.cv, cx, ty, label, label_w - 10 * u, label_ink);
                    self.text.draw(&mut d.cv, cx + label_w, ty, value, cw - label_w, value_ink);
                }
                Row::Check(label, action) => {
                    let index = item_at(Item::Check(*action));
                    if index.is_some() && (d.hover == index || Some(d.focus) == index) {
                        d.cv.fill(cx - 4 * u, y, cw + 8 * u, row_h, if d.hover == index { HOVER } else { 0xeef4fc });
                    }
                    let (box_size, by) = (14 * u, y + (row_h - 14 * u) / 2);
                    let on = self.checked(*action);
                    d.cv.fill(cx, by, box_size, box_size, if on { SELECT } else { WHITE });
                    d.cv.frame(cx, by, box_size, box_size, line, if on { SELECT } else { 0x8a8a8a });
                    if on {
                        // A tick: a short stroke down-right, a long one up-right.
                        for i in 0..4 * u {
                            d.cv.fill(cx + 3 * u + i, by + 7 * u + i, 2 * line, 2 * line, ON_ACCENT);
                        }
                        for i in 0..6 * u {
                            d.cv.fill(cx + 6 * u + i, by + 10 * u - i, 2 * line, 2 * line, ON_ACCENT);
                        }
                    }
                    self.text.draw(&mut d.cv, cx + 24 * u, ty, label, cw - 24 * u, TEXT);
                }
                Row::Button(label, action) => {
                    let Some(index) = item_at(Item::RowButton(*action)) else { continue };
                    let (x, by, bw, bh, _) = layout.items[index];
                    d.cv.fill(x, by, bw, bh, if d.hover == Some(index) { 0xdcdcdc } else { 0xececec });
                    d.cv.frame(x, by, bw, bh, line, if d.focus == index { 0x5b9bea } else { 0xa8a8a8 });
                    self.text.draw(&mut d.cv, x + 14 * u, by + (bh - self.text.height) / 2, label, bw, TEXT);
                }
                Row::ListItem(label, action) => {
                    // One line of a white list box; the chosen line is filled.
                    let index = item_at(Item::Check(*action));
                    let chosen = self.checked(*action);
                    let fill = if chosen { SELECT } else if d.hover == index { HOVER } else { WHITE };
                    d.cv.fill(cx, y, cw, row_h, fill);
                    d.cv.fill(cx, y, line, row_h, 0x9a9a9a);
                    d.cv.fill(cx + cw - line, y, line, row_h, 0x9a9a9a);
                    self.text.draw(&mut d.cv, cx + 8 * u, ty, label, cw - 16 * u, if chosen { ON_ACCENT } else { TEXT });
                }
                Row::Input(hint, action) => {
                    let focused = item_at(Item::Input(*action)) == Some(d.focus);
                    d.cv.fill(cx, y, cw, row_h, WHITE);
                    d.cv.frame(cx, y, cw, row_h, line, if focused { 0x5b9bea } else { 0x9a9a9a });
                    // A long text shows its end, where typing happens.
                    let room = cw - 16 * u;
                    let mut shown = self.dialog_input.as_str();
                    while self.text.width(shown) > room {
                        shown = &shown[shown.chars().next().map_or(1, char::len_utf8)..];
                    }
                    if self.dialog_input.is_empty() {
                        self.text.draw(&mut d.cv, cx + 8 * u, ty, hint, room, 0x9a9a9a);
                    } else {
                        self.text.draw(&mut d.cv, cx + 8 * u, ty, shown, room + 4 * u, TEXT);
                    }
                    if focused {
                        let caret = cx + 8 * u + if self.dialog_input.is_empty() { 0 } else { self.text.width(shown) + u };
                        d.cv.fill(caret, ty, line, self.text.height, TEXT);
                    }
                }
                Row::Gap => {}
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }
        // Close each list box with a top and a bottom edge.
        let mut y = layout.top;
        for (i, row) in d.rows.iter().enumerate() {
            let is_item = |r: Option<&Row>| matches!(r, Some(Row::ListItem(..)));
            if is_item(Some(row)) {
                if i == 0 || !is_item(d.rows.get(i - 1)) {
                    d.cv.fill(cx, y, cw, line, 0x9a9a9a);
                }
                if !is_item(d.rows.get(i + 1)) {
                    d.cv.fill(cx, y + row_h - line, cw, line, 0x9a9a9a);
                }
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }

        let last = d.buttons.len().saturating_sub(1);
        for (index, &(x, by, bw, bh, item)) in layout.items.iter().enumerate() {
            let Item::Button(i) = item else { continue };
            let label = d.buttons[i].0;
            // The last button is the main action, filled; the rest are plain.
            let main = i == last && d.buttons[i].1.is_some();
            let (fill, ink) = match (main, d.danger, d.hover == Some(index)) {
                (true, true, hovered) => (if hovered { 0xb3261e } else { 0xd93025 }, ON_ACCENT),
                (true, false, hovered) => (if hovered { 0x1557ad } else { SELECT }, ON_ACCENT),
                (false, _, true) => (0xdcdcdc, TEXT),
                (false, _, false) => (0xececec, TEXT),
            };
            d.cv.fill(x, by, bw, bh, fill);
            d.cv.frame(x, by, bw, bh, line, if main { fill } else { 0xa8a8a8 });
            if d.focus == index {
                d.cv.frame(x - 3 * u, by - 3 * u, bw + 6 * u, bh + 6 * u, line, 0x5b9bea); // focus ring
            }
            let lx = x + (bw - self.text.width(label)) / 2;
            self.text.draw(&mut d.cv, lx, by + (bh - self.text.height) / 2, label, bw, ink);
        }

        self.c.put_image(d.win, self.gc, d.cv.w, 0, &d.cv.px)?;
        self.c.flush()?;
        self.dialog = Some(d);
        Ok(())
    }

    /// Runs the item at `index`.
    fn dialog_activate(&mut self, index: usize, time: u32) {
        let Some(d) = self.dialog.take() else { return };
        let item = self.dialog_layout(&d).items.get(index).map(|i| i.4);
        let stay = match item {
            // These act and leave the dialog open.
            Some(Item::Check(action) | Item::RowButton(action) | Item::Input(action)) => Some(action),
            Some(Item::Nav(i)) => Some(d.nav[i].action),
            _ => None,
        };
        let button = match item {
            Some(Item::Button(i)) => Some(d.buttons[i].1),
            _ => None,
        };
        self.dialog = Some(d);
        if let Some(action) = stay {
            self.act(action, time);
            self.refresh_option_states();
            // A page of the Options window is rebuilt: what it shows may
            // depend on what was just changed.
            if self.paged_dialog_open() {
                let _ = self.open_options(self.options_page.unwrap_or(0));
            }
            return;
        }
        let Some(action) = button else { return };
        if self.paged_dialog_open() {
            // The Options window: act while its pages still hold the
            // edits, then close unless this was Apply.
            if let Some(action) = action {
                self.act(action, time);
            }
            if action == Some(Action::OptionsApply) {
                let _ = self.open_options(self.options_page.unwrap_or(0));
            } else {
                self.close_dialog();
            }
            return;
        }
        // Any other dialog: a button closes it, then does its action.
        self.close_dialog();
        if let Some(action) = action {
            self.act(action, time);
        }
    }

    /// Handles an event addressed to the dialog window.
    pub fn dialog_event(&mut self, ev: &[u8]) -> io::Result<()> {
        let Some(d) = self.dialog.take() else { return Ok(()) };
        let items = self.dialog_layout(&d).items;
        let focus = d.focus;
        self.dialog = Some(d);
        let count = items.len().max(1);
        let at = |i| u16at(ev, i) as i16 as i32;
        let hit = |x: i32, y: i32| items.iter().position(|&(ix, iy, iw, ih, _)| x >= ix && x < ix + iw && y >= iy && y < iy + ih);
        let time = u32at(ev, 4);
        match ev[0] & 0x7f {
            EV_CLIENT_MESSAGE if u32at(ev, 12) == self.wm_delete => self.close_dialog(),
            EV_MOTION => {
                let hover = hit(at(24), at(26));
                if let Some(d) = self.dialog.as_mut() {
                    if d.hover == hover {
                        return Ok(());
                    }
                    d.hover = hover;
                }
            }
            EV_BUTTON_PRESS if ev[1] == 1 => {
                if let Some(index) = hit(at(24), at(26)) {
                    self.dialog.as_mut().unwrap().focus = index;
                    self.dialog_activate(index, time);
                }
            }
            EV_KEY_PRESS => {
                let state = u16at(ev, 28);
                let set_focus = |app: &mut App, f: usize| app.dialog.as_mut().unwrap().focus = f;
                // With the text field focused, keys type into it.
                let typing = matches!(items.get(focus), Some((.., Item::Input(_))));
                match self.c.key(ev[1], state) {
                    Key::Char(ch) if typing && state & MOD_CTRL != 0 => {
                        if ch.eq_ignore_ascii_case(&'v') {
                            // Paste: the text arrives as a SelectionNotify.
                            let body = [W(self.win), W(self.clipboard), W(self.utf8_string), W(self.paste_prop), W(0)];
                            self.c.req(24, 0, &body);
                        }
                        return Ok(());
                    }
                    Key::Char(ch) if typing && !ch.is_control() => self.dialog_input.push(ch),
                    Key::Sym(K_BACKSPACE) if typing => drop(self.dialog_input.pop()),
                    Key::Sym(K_ESCAPE) => self.close_dialog(),
                    Key::Sym(K_RETURN | K_KP_ENTER) | Key::Char(' ') => self.dialog_activate(focus, time),
                    Key::Sym(K_TAB) if state & MOD_SHIFT != 0 => set_focus(self, (focus + count - 1) % count),
                    Key::Sym(K_BACK_TAB | K_LEFT | K_UP) => set_focus(self, (focus + count - 1) % count),
                    Key::Sym(K_TAB | K_RIGHT | K_DOWN) => set_focus(self, (focus + 1) % count),
                    _ => return Ok(()),
                }
            }
            _ => {} // Expose: just redraw
        }
        self.render_dialog()
    }
}

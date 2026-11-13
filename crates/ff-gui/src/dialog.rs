//! A reusable dialog window: a titled, modal window over the main one,
//! made of content rows (text, label/value pairs, checkboxes) and buttons.
//! Confirmations and settings screens are all built from it.

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
    /// Half a line of space.
    Gap,
}

/// A button: its label and what it does (`None` just closes the dialog).
pub type Button = (&'static str, Option<Action>);

/// Something in the dialog the user can activate.
#[derive(Clone, Copy, PartialEq)]
enum Item {
    Check(Action),
    Button(usize),
}

pub struct Dialog {
    pub win: u32,
    pub cv: Canvas,
    rows: Vec<Row>,
    buttons: Vec<Button>,
    /// The last button is the main one; draw it as dangerous (red).
    danger: bool,
    /// Index into the activatable items (checkboxes, then buttons).
    focus: usize,
    hover: Option<usize>,
}

impl App {
    /// Opens a dialog. `focus_button` is the button Enter triggers at first.
    pub fn open_dialog(
        &mut self,
        title: &str,
        rows: Vec<Row>,
        buttons: Vec<Button>,
        focus_button: usize,
        danger: bool,
    ) -> io::Result<()> {
        self.close_dialog();
        self.menu = None;
        let checks = rows.iter().filter(|r| matches!(r, Row::Check(..))).count();
        let mut dialog = Dialog {
            win: self.c.new_id(),
            cv: Canvas::new(),
            rows,
            buttons,
            danger,
            focus: checks + focus_button,
            hover: None,
        };
        let (w, h, _) = self.dialog_layout(&dialog);
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
        }
    }

    /// Size of the dialog and the rectangle (x, y, w, h) of every
    /// activatable item, checkboxes first, then buttons.
    fn dialog_layout(&mut self, d: &Dialog) -> (i32, i32, Vec<(i32, i32, i32, i32, Item)>) {
        let (u, row_h) = (self.u, self.row_h());
        let pad = 18 * u;
        let content = d
            .rows
            .iter()
            .map(|row| match row {
                Row::Text(t) | Row::Dim(t) => self.text.width(t),
                Row::Pair(_, value) => 120 * u + self.text.width(value),
                Row::Check(label, _) => 28 * u + self.text.width(label),
                Row::Gap => 0,
            })
            .max()
            .unwrap_or(0);
        let button_w: Vec<i32> = d.buttons.iter().map(|(label, _)| self.text.width(label) + 36 * u).collect();
        let buttons_total = button_w.iter().sum::<i32>() + 10 * u * (button_w.len() as i32 - 1).max(0);
        let w = (content.max(buttons_total) + 2 * pad).clamp(380 * u, 760 * u);

        let mut items = Vec::new();
        let mut y = pad;
        for row in &d.rows {
            if let Row::Check(_, action) = row {
                items.push((pad - 4 * u, y, w - 2 * pad + 8 * u, row_h, Item::Check(*action)));
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }
        y += 14 * u;
        let button_h = row_h + 8 * u;
        let mut x = w - pad - buttons_total;
        for (i, bw) in button_w.into_iter().enumerate() {
            items.push((x, y, bw, button_h, Item::Button(i)));
            x += bw + 10 * u;
        }
        (w, y + button_h + pad, items)
    }

    pub fn render_dialog(&mut self) -> io::Result<()> {
        let Some(mut d) = self.dialog.take() else { return Ok(()) };
        let (u, row_h) = (self.u, self.row_h());
        let pad = 18 * u;
        let (w, h, items) = self.dialog_layout(&d);
        let line = u.max(1);
        d.cv.fill(0, 0, w, h, WHITE);

        let mut y = pad;
        for row in &d.rows {
            let ty = y + 3 * u;
            match row {
                Row::Text(t) => self.text.draw(&mut d.cv, pad, ty, t, w - 2 * pad, TEXT),
                Row::Dim(t) => self.text.draw(&mut d.cv, pad, ty, t, w - 2 * pad, DIM),
                Row::Pair(label, value) => {
                    self.text.draw(&mut d.cv, pad, ty, label, 110 * u, DIM);
                    self.text.draw(&mut d.cv, pad + 120 * u, ty, value, w - 2 * pad - 120 * u, TEXT);
                }
                Row::Check(label, action) => {
                    let index = items.iter().position(|i| i.4 == Item::Check(*action));
                    if index.is_some() && (d.hover == index || d.focus == index.unwrap()) {
                        d.cv.fill(pad - 4 * u, y, w - 2 * pad + 8 * u, row_h, if d.hover == index { HOVER } else { 0xeef4fc });
                    }
                    let (box_size, by) = (14 * u, y + (row_h - 14 * u) / 2);
                    let on = self.checked(*action);
                    d.cv.fill(pad, by, box_size, box_size, if on { SELECT } else { WHITE });
                    d.cv.frame(pad, by, box_size, box_size, line, if on { SELECT } else { 0x8a8a8a });
                    if on {
                        // A tick: a short stroke down-right, a long one up-right.
                        for i in 0..4 * u {
                            d.cv.fill(pad + 3 * u + i, by + 7 * u + i, 2 * line, 2 * line, WHITE);
                        }
                        for i in 0..6 * u {
                            d.cv.fill(pad + 6 * u + i, by + 10 * u - i, 2 * line, 2 * line, WHITE);
                        }
                    }
                    self.text.draw(&mut d.cv, pad + 24 * u, ty, label, w - 2 * pad - 24 * u, TEXT);
                }
                Row::Gap => {}
            }
            y += if matches!(row, Row::Gap) { row_h / 2 } else { row_h };
        }

        let last = d.buttons.len().saturating_sub(1);
        for (index, &(x, by, bw, bh, item)) in items.iter().enumerate() {
            let Item::Button(i) = item else { continue };
            let label = d.buttons[i].0;
            // The last button is the main action, filled; the rest are plain.
            let main = i == last && d.buttons[i].1.is_some();
            let (fill, ink) = match (main, d.danger, d.hover == Some(index)) {
                (true, true, hovered) => (if hovered { 0xb3261e } else { 0xd93025 }, WHITE),
                (true, false, hovered) => (if hovered { 0x1557ad } else { SELECT }, WHITE),
                (false, _, true) => (0xdcdcdc, TEXT),
                (false, _, false) => (PANEL, TEXT),
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

    /// Runs the item at `index`: toggles a checkbox, or presses a button.
    fn dialog_activate(&mut self, index: usize, time: u32) {
        let Some(d) = self.dialog.take() else { return };
        let (_, _, items) = self.dialog_layout(&d);
        let button = match items.get(index).map(|i| i.4) {
            Some(Item::Check(action)) => {
                self.dialog = Some(d);
                self.act(action, time);
                return self.refresh_option_states();
            }
            Some(Item::Button(i)) => d.buttons[i].1,
            None => {
                self.dialog = Some(d);
                return;
            }
        };
        // A button closes the dialog, then does its action.
        self.dialog = Some(d);
        self.close_dialog();
        if let Some(action) = button {
            self.act(action, time);
        }
    }

    /// Handles an event addressed to the dialog window.
    pub fn dialog_event(&mut self, ev: &[u8]) -> io::Result<()> {
        let Some(d) = self.dialog.take() else { return Ok(()) };
        let (_, _, items) = self.dialog_layout(&d);
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
                match self.c.key(ev[1], state) {
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

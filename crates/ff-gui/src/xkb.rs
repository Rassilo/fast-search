//! Just enough of the XKB text keymap format (the one a Wayland compositor
//! hands to its clients) to turn key codes into characters and named keys:
//! key codes, key types and symbols. Compose sequences and dead keys are
//! not handled.

use std::collections::HashMap;

use crate::x11::*;

pub const SHIFT: u32 = 1;
pub const LOCK: u32 = 2;
pub const CTRL: u32 = 4;
pub const ALT: u32 = 8;
const NUM: u32 = 0x10;
const LEVEL3: u32 = 0x80;

/// Key symbols by name: "eacute e9", or "Cyrillic_a 6c1 430" where the
/// character differs from the symbol's number.
static NAMES: &str = include_str!("keysyms.txt");

const NUM_LOCK: u32 = 0xff7f;
const LEVEL3_SHIFT: u32 = 0xfe03;
const LEVEL5_SHIFT: u32 = 0xfe11;

#[derive(Clone, Copy)]
struct Level {
    key: Key,
    keypad: bool,
}

/// How modifiers choose a level of a key.
#[derive(Clone, Default)]
struct KeyType {
    /// The modifiers this type looks at.
    mask: u32,
    /// Exact modifier combinations and the level (from 0) each selects.
    map: Vec<(u32, usize)>,
}

struct Group {
    ty: KeyType,
    levels: Vec<Level>,
}

#[derive(Default)]
pub struct Keymap {
    /// Key code to its layouts ("groups").
    keys: HashMap<u32, Vec<Group>>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum T<'a> {
    Id(&'a str),
    Str(&'a str),
    /// A key name: `<AD01>`.
    Code(&'a str),
    P(u8),
}

fn tokenize(text: &str) -> Vec<T<'_>> {
    let b = text.as_bytes();
    let find = |from: usize, byte: u8| b[from..].iter().position(|&c| c == byte).map_or(b.len(), |p| from + p);
    let (mut i, mut out) = (0, Vec::new());
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' | 0 => i += 1,
            b'#' => i = find(i, b'\n'),
            b'/' if b.get(i + 1) == Some(&b'/') => i = find(i, b'\n'),
            quote @ (b'"' | b'<') => {
                let end = find(i + 1, if quote == b'"' { b'"' } else { b'>' });
                let inner = text.get(i + 1..end).unwrap_or("");
                out.push(if quote == b'"' { T::Str(inner) } else { T::Code(inner) });
                i = end + 1;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                out.push(T::Id(&text[start..i]));
            }
            c => {
                out.push(T::P(c));
                i += 1;
            }
        }
    }
    out
}

/// Index just past the `;` that ends the statement starting at `i`, or of
/// the `}` that closes the enclosing block if that comes first.
fn skip_statement(t: &[T], mut i: usize) -> usize {
    let mut depth = 0;
    while let Some(tok) = t.get(i) {
        match tok {
            T::P(b'{' | b'[' | b'(') => depth += 1,
            T::P(b'}' | b']' | b')') if depth == 0 => return i,
            T::P(b'}' | b']' | b')') => depth -= 1,
            T::P(b';') if depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    i
}

/// Index of the first token inside the block that opens at or after `i`.
fn enter_block(t: &[T], mut i: usize) -> usize {
    while i < t.len() && t[i] != T::P(b'{') {
        i += 1;
    }
    i + 1
}

/// Modifier names joined by `+`, up to a token that is neither.
fn modifier_names<'a>(t: &[T<'a>], i: &mut usize) -> Vec<&'a str> {
    let mut names = Vec::new();
    loop {
        match t.get(*i) {
            Some(T::Id(name)) => names.push(*name),
            Some(T::P(b'+')) => {}
            _ => return names,
        }
        *i += 1;
    }
}

fn real_modifier(name: &str) -> Option<u32> {
    let names = ["Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5"];
    names.iter().position(|n| n.eq_ignore_ascii_case(name)).map(|bit| 1 << bit)
}

/// "Group2" or "2" as an index from 0.
fn index_of(name: &str) -> usize {
    name.trim_start_matches(|c: char| !c.is_ascii_digit()).parse::<usize>().unwrap_or(1).saturating_sub(1)
}

struct RawType<'a> {
    mask: Vec<&'a str>,
    map: Vec<(Vec<&'a str>, usize)>,
}

#[derive(Default)]
struct RawKey<'a> {
    /// Per group: its type, if named, and the names of its levels.
    groups: Vec<(Option<&'a str>, Vec<&'a str>)>,
    /// A type given for all groups.
    ty: Option<&'a str>,
}

impl<'a> RawKey<'a> {
    fn group(&mut self, index: usize) -> &mut (Option<&'a str>, Vec<&'a str>) {
        let index = index.min(7);
        if self.groups.len() <= index {
            self.groups.resize(index + 1, (None, Vec::new()));
        }
        &mut self.groups[index]
    }
}

/// The level names of `[ a, A, { b, c } ]`; `i` is just past the `[`.
fn level_names<'a>(t: &[T<'a>], i: &mut usize) -> Vec<&'a str> {
    let mut names = Vec::new();
    while let Some(tok) = t.get(*i) {
        *i += 1;
        match tok {
            T::Id(name) => names.push(*name),
            T::P(b']') => break,
            T::P(b'{') => {
                // Several symbols on one level: the first one will do.
                names.push(if let Some(T::Id(name)) = t.get(*i) { *name } else { "NoSymbol" });
                while *i < t.len() && t[*i] != T::P(b'}') {
                    *i += 1;
                }
                *i += 1;
            }
            _ => {}
        }
    }
    names
}

/// The body of `key <NAME> { ... }`; `i` is just past the `{`.
fn key_body<'a>(t: &[T<'a>], i: &mut usize) -> RawKey<'a> {
    let mut key = RawKey::default();
    let mut implicit = 0;
    // `name[GroupN]`: the group index, if one is given.
    let group_index = |i: &mut usize| {
        if t.get(*i) != Some(&T::P(b'[')) {
            return None;
        }
        let index = if let Some(T::Id(name)) = t.get(*i + 1) { index_of(name) } else { 0 };
        *i += 3;
        Some(index)
    };
    while let Some(tok) = t.get(*i) {
        *i += 1;
        match tok {
            T::P(b'}') => break,
            T::P(b'[') => {
                key.group(implicit).1 = level_names(t, i);
                implicit += 1;
            }
            T::Id(word) if word.eq_ignore_ascii_case("type") => {
                let group = group_index(i);
                if let (Some(T::P(b'=')), Some(T::Str(name))) = (t.get(*i), t.get(*i + 1)) {
                    match group {
                        Some(g) => key.group(g).0 = Some(*name),
                        None => key.ty = Some(*name),
                    }
                    *i += 2;
                }
            }
            T::Id(word) if word.eq_ignore_ascii_case("symbols") => {
                let group = group_index(i).unwrap_or(0);
                if t.get(*i) == Some(&T::P(b'=')) && t.get(*i + 1) == Some(&T::P(b'[')) {
                    *i += 2;
                    key.group(group).1 = level_names(t, i);
                }
            }
            T::Id(_) => {
                // Something else (actions, repeat, ...): skip to the next field.
                let mut depth = 0;
                while let Some(tok) = t.get(*i) {
                    match tok {
                        T::P(b'{' | b'[' | b'(') => depth += 1,
                        T::P(b'}') if depth == 0 => break,
                        T::P(b'}' | b']' | b')') => depth -= 1,
                        T::P(b',') if depth == 0 => break,
                        _ => {}
                    }
                    *i += 1;
                }
            }
            _ => {}
        }
    }
    key
}

/// The key symbol a level is written as: a name ("eacute", "U20AC") or,
/// from newer compositors, a number ("0xe9").
fn keysym(name: &str, names: &HashMap<&str, u32>) -> u32 {
    let hex = |digits: &str| u32::from_str_radix(digits, 16).ok();
    if let Some(value) = name.strip_prefix("0x").and_then(hex) {
        return value;
    }
    if let Some(&value) = names.get(name) {
        return value;
    }
    // "U20AC": a Unicode character directly.
    name.strip_prefix('U').filter(|d| d.len() >= 4).and_then(hex).map_or(0, |code| 0x0100_0000 + code)
}

/// The key a key symbol stands for.
fn level_of(sym: u32, chars: &HashMap<u32, char>) -> Level {
    let ch = |code: u32| char::from_u32(code).filter(|c| !c.is_control()).map_or(Key::None, Key::Char);
    let key = match sym {
        0xff08 | 0xff09 | 0xff0d | 0xff1b | 0xff50..=0xff57 | 0xff63 | 0xff67 | 0xffff | 0xffbe..=0xffc9 | 0xfe20 | 0xff8d => Key::Sym(sym),
        // The keypad without Num Lock: the navigation keys.
        0xff95..=0xff9c => Key::Sym([K_HOME, K_LEFT, K_UP, K_RIGHT, K_DOWN, K_PAGE_UP, K_PAGE_DOWN, K_END][(sym - 0xff95) as usize]),
        0xff9e => Key::Sym(0xff63),
        0xff9f => Key::Sym(K_DELETE),
        // The keypad's characters.
        0xff80 => Key::Char(' '),
        0xffaa..=0xffaf => Key::Char(b"*+,-./"[(sym - 0xffaa) as usize] as char),
        0xffb0..=0xffb9 => ch(sym - 0xffb0 + '0' as u32),
        0xffbd => Key::Char('='),
        0x20..=0x7e | 0xa0..=0xff => ch(sym),
        0x0100_0000..=0x0110_ffff => ch(sym - 0x0100_0000),
        _ => chars.get(&sym).map_or(Key::None, |c| Key::Char(*c)), // else a modifier, dead key, media key
    };
    Level { key, keypad: (0xff80..=0xffbd).contains(&sym) }
}

/// The type XKB gives a key that names none, judged by its symbols.
fn automatic_type(levels: &[Level]) -> &'static str {
    let letter_pair = |a: usize, b: usize| match (levels.get(a).map(|l| l.key), levels.get(b).map(|l| l.key)) {
        (Some(Key::Char(lower)), Some(Key::Char(upper))) => lower.is_lowercase() && upper.is_uppercase(),
        _ => false,
    };
    let keypad = levels.iter().take(2).any(|l| l.keypad);
    match levels.len() {
        0 | 1 => "ONE_LEVEL",
        2 if letter_pair(0, 1) => "ALPHABETIC",
        2 if keypad => "KEYPAD",
        2 => "TWO_LEVEL",
        _ if letter_pair(0, 1) && letter_pair(2, 3) => "FOUR_LEVEL_ALPHABETIC",
        _ if letter_pair(0, 1) => "FOUR_LEVEL_SEMIALPHABETIC",
        _ if keypad => "FOUR_LEVEL_KEYPAD",
        _ => "FOUR_LEVEL",
    }
}

/// The standard types, for a keymap that does not define the one needed.
fn standard_type(name: &str) -> KeyType {
    let four = vec![(SHIFT, 1), (LEVEL3, 2), (SHIFT | LEVEL3, 3)];
    match name {
        "ONE_LEVEL" => KeyType::default(),
        "ALPHABETIC" => KeyType { mask: SHIFT | LOCK, map: vec![(SHIFT, 1), (LOCK, 1)] },
        "KEYPAD" => KeyType { mask: SHIFT | NUM, map: vec![(SHIFT, 1), (NUM, 1)] },
        "TWO_LEVEL" => KeyType { mask: SHIFT, map: vec![(SHIFT, 1)] },
        _ => KeyType { mask: SHIFT | LEVEL3, map: four },
    }
}

impl Keymap {
    pub fn parse(text: &str) -> Keymap {
        let t = tokenize(text);
        let mut codes: HashMap<&str, u32> = HashMap::new();
        let mut aliases: Vec<(&str, &str)> = Vec::new();
        let mut types: HashMap<&str, RawType> = HashMap::new();
        let mut keys: Vec<(&str, RawKey)> = Vec::new();
        let mut modifier_map: Vec<(u32, &str)> = Vec::new();
        // Virtual modifiers and the real ones they usually stand for; the
        // keymap's own statements below correct these.
        let mut virtual_mods: HashMap<&str, u32> = HashMap::from([
            ("NumLock", NUM),
            ("LevelThree", LEVEL3),
            ("AltGr", LEVEL3),
            ("Alt", ALT),
            ("Meta", ALT),
            ("LevelFive", 0x20),
            ("Super", 0x40),
            ("Hyper", 0x40),
        ]);

        let mut i = 0;
        while i < t.len() {
            let T::Id(section) = t[i] else {
                i += 1;
                continue;
            };
            i += 1;
            if !matches!(section, "xkb_keycodes" | "xkb_types" | "xkb_compatibility" | "xkb_symbols" | "xkb_geometry") {
                continue;
            }
            i = enter_block(&t, i);
            loop {
                match (section, t.get(i).copied(), t.get(i + 1).copied(), t.get(i + 2).copied()) {
                    (_, None, ..) => break,
                    (_, Some(T::P(b'}')), ..) => {
                        i += 1;
                        break;
                    }
                    ("xkb_keycodes", Some(T::Code(name)), Some(T::P(b'=')), Some(T::Id(number))) => {
                        if let Ok(code) = number.parse() {
                            codes.insert(name, code);
                        }
                        i = skip_statement(&t, i);
                    }
                    ("xkb_keycodes", Some(T::Id("alias")), Some(T::Code(alias)), _) => {
                        if let Some(T::Code(target)) = t.get(i + 3) {
                            aliases.push((alias, target));
                        }
                        i = skip_statement(&t, i);
                    }
                    ("xkb_types", Some(T::Id("virtual_modifiers")), ..) => {
                        // `virtual_modifiers NumLock=Mod2, Alt, ...;`
                        let end = skip_statement(&t, i);
                        let mut j = i + 1;
                        while j < end {
                            if let (T::Id(name), Some(T::P(b'=')), Some(T::Id(value))) = (t[j], t.get(j + 1), t.get(j + 2)) {
                                let mask = real_modifier(value).or_else(|| {
                                    let digits = value.strip_prefix("0x")?;
                                    u32::from_str_radix(digits, 16).ok()
                                });
                                if let Some(mask) = mask.filter(|m| *m != 0) {
                                    virtual_mods.insert(name, mask & 0xff);
                                }
                                j += 2;
                            }
                            j += 1;
                        }
                        i = end;
                    }
                    ("xkb_types", Some(T::Id("type")), Some(T::Str(name)), _) => {
                        let mut raw = RawType { mask: Vec::new(), map: Vec::new() };
                        i = enter_block(&t, i);
                        loop {
                            match (t.get(i).copied(), t.get(i + 1).copied()) {
                                (None, _) => break,
                                (Some(T::P(b'}')), _) => {
                                    i += 1;
                                    break;
                                }
                                (Some(T::Id("modifiers")), Some(T::P(b'='))) => {
                                    i += 2;
                                    raw.mask = modifier_names(&t, &mut i);
                                }
                                (Some(T::Id("map")), Some(T::P(b'['))) => {
                                    i += 2;
                                    let mods = modifier_names(&t, &mut i);
                                    if let (Some(T::P(b']')), Some(T::P(b'=')), Some(T::Id(level))) = (t.get(i), t.get(i + 1), t.get(i + 2)) {
                                        raw.map.push((mods, index_of(level)));
                                    }
                                }
                                _ => {}
                            }
                            let next = skip_statement(&t, i);
                            i = if next == i { i + 1 } else { next };
                        }
                        types.insert(name, raw);
                        i = skip_statement(&t, i);
                    }
                    ("xkb_symbols", Some(T::Id("key")), Some(T::Code(name)), _) => {
                        i = enter_block(&t, i);
                        keys.push((name, key_body(&t, &mut i)));
                        i = skip_statement(&t, i);
                    }
                    ("xkb_symbols", Some(T::Id("modifier_map")), Some(T::Id(modifier)), _) => {
                        let end = skip_statement(&t, i);
                        if let Some(mask) = real_modifier(modifier) {
                            modifier_map.extend(t[i..end].iter().filter_map(|tok| match tok {
                                T::Code(key) => Some((mask, *key)),
                                _ => None,
                            }));
                        }
                        i = end;
                    }
                    _ => {
                        let next = skip_statement(&t, i);
                        // A stray closing bracket would stop us for good.
                        i = if next == i { i + 1 } else { next };
                    }
                }
            }
        }

        let (mut names, mut chars): (HashMap<&str, u32>, HashMap<u32, char>) = (HashMap::new(), HashMap::new());
        for line in NAMES.lines() {
            let mut fields = line.split(' ');
            let (Some(name), Some(sym)) = (fields.next(), fields.next().and_then(|v| u32::from_str_radix(v, 16).ok())) else { continue };
            names.insert(name, sym);
            if let Some(ch) = fields.next().and_then(|v| u32::from_str_radix(v, 16).ok()).and_then(char::from_u32) {
                chars.insert(sym, ch);
            }
        }

        // Where the keymap says which real modifier a lock or level key
        // sets, believe it over the usual assignment.
        let unalias = |name: &'_ str| aliases.iter().find(|(a, _)| *a == name).map_or(name.to_string(), |(_, target)| target.to_string());
        let mut learned: HashMap<&str, u32> = HashMap::new();
        for (mask, key) in &modifier_map {
            let key = unalias(key);
            let Some((_, raw)) = keys.iter().find(|(name, _)| *name == key) else { continue };
            for symbol in raw.groups.iter().flat_map(|(_, levels)| levels.first()) {
                let which = match keysym(symbol, &names) {
                    NUM_LOCK => "NumLock",
                    LEVEL3_SHIFT => "LevelThree",
                    LEVEL5_SHIFT => "LevelFive",
                    _ => continue,
                };
                *learned.entry(which).or_insert(0) |= mask;
            }
        }
        virtual_mods.extend(learned);

        let mask_of = |names: &[&str]| names.iter().fold(0, |mask, name| mask | real_modifier(name).or_else(|| virtual_mods.get(name).copied()).unwrap_or(0));
        let types: HashMap<&str, KeyType> = types
            .iter()
            .map(|(name, raw)| {
                let map = raw.map.iter().map(|(mods, level)| (mask_of(mods), *level)).collect();
                (*name, KeyType { mask: mask_of(&raw.mask), map })
            })
            .collect();

        let mut map = Keymap::default();
        for (name, raw) in &keys {
            let Some(&code) = codes.get(name).or_else(|| codes.get(unalias(name).as_str())) else { continue };
            let groups = raw
                .groups
                .iter()
                .map(|(ty, symbols)| {
                    let levels: Vec<Level> = symbols.iter().map(|n| level_of(keysym(n, &names), &chars)).collect();
                    let name = ty.or(raw.ty).unwrap_or_else(|| automatic_type(&levels));
                    Group { ty: types.get(name).cloned().unwrap_or_else(|| standard_type(name)), levels }
                })
                .collect();
            map.keys.insert(code, groups);
        }
        map
    }

    fn lookup(&self, groups: &[Group], group: usize, mods: u32) -> Key {
        let Some(group) = groups.get(group).or(groups.first()) else { return Key::None };
        let active = mods & group.ty.mask;
        let level = group.ty.map.iter().find(|(m, _)| *m == active).map_or(0, |(_, level)| *level);
        let mut key = group.levels.get(level).map_or(Key::None, |l| l.key);
        if key == Key::None {
            key = group.levels.first().map_or(Key::None, |l| l.key);
        }
        // Caps Lock, where the key's type does not deal with it itself.
        if let Key::Char(ch) = key {
            if mods & LOCK != 0 && group.ty.mask & LOCK == 0 && ch.is_lowercase() {
                key = Key::Char(ch.to_uppercase().next().unwrap_or(ch));
            }
        }
        key
    }

    /// The key for a key code (as in X11: the evdev code plus 8) under
    /// these modifiers and this layout.
    pub fn key(&self, code: u32, mods: u32, group: u32) -> Key {
        let Some(groups) = self.keys.get(&code) else { return Key::None };
        let key = self.lookup(groups, group as usize, mods);
        // Ctrl+C must work on a non-Latin layout too: take the letter the
        // key has on another layout.
        if mods & CTRL != 0 && matches!(key, Key::Char(c) if !c.is_ascii()) {
            let latin = (0..groups.len()).map(|g| self.lookup(groups, g, mods)).find(|k| matches!(k, Key::Char(c) if c.is_ascii()));
            return latin.unwrap_or(key);
        }
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"xkb_keymap {
xkb_keycodes "(unnamed)" {
	minimum = 8;
	maximum = 255;
	<ESC> = 9;
	<AE01> = 10;
	<AD01> = 24;
	<AD03> = 26;
	<LFSH> = 50;
	<NMLK> = 77;
	<KP7> = 79;
	<LVL3> = 92;
	<BKSP> = 22;
	<AD02> = 25;
	<UP> = 111;
	<AC01> = 38;
	indicator 1 = "Caps Lock";
	alias <LatQ> = <AD01>;
};
xkb_types "(unnamed)" {
	virtual_modifiers NumLock,Alt,LevelThree;
	type "ONE_LEVEL" { modifiers= none; level_name[Level1]= "Any"; };
	type "TWO_LEVEL" { modifiers= Shift; map[Shift]= Level2; };
	type "ALPHABETIC" { modifiers= Shift+Lock; map[Shift]= Level2; map[Lock]= Level2; };
	type "KEYPAD" { modifiers= Shift+NumLock; map[Shift]= Level2; map[NumLock]= Level2; };
	type "FOUR_LEVEL_SEMIALPHABETIC" {
		modifiers= Shift+Lock+LevelThree;
		map[Shift]= Level2; map[Lock]= Level2; map[LevelThree]= Level3;
		map[Shift+LevelThree]= Level4; map[Lock+LevelThree]= Level3;
		preserve[Lock+LevelThree]= Lock;
	};
};
xkb_compatibility "(unnamed)" {
	interpret Any+AnyOf(all) { action= SetMods(modifiers=modMapMods,clearLocks); };
};
xkb_symbols "(unnamed)" {
	name[group1]="French";
	key <ESC> { [ Escape ] };
	key <BKSP> { [ BackSpace, BackSpace ] };
	key <AE01> { [ ampersand, 1, onesuperior, exclamdown ] };
	key <AD01> {
		type[group1]= "ALPHABETIC",
		symbols[Group1]= [ a, A ],
		symbols[Group2]= [ Cyrillic_ef, Cyrillic_EF ]
	};
	key <AD03> { type= "FOUR_LEVEL_SEMIALPHABETIC", symbols[Group1]= [ e, E, EuroSign, cent ], actions[Group1]= [ NoAction(), SetMods(modifiers=Shift) ] };
	key <LFSH> { [ Shift_L ] };
	key <NMLK> { [ 0xff7f ] };
	key <AD02> {	[ 0x7a, 0x5a ] };
	key <UP> { [ 0xff52 ] };
	key <AC01> { symbols[1]= [ 0x6c1, 0x6e1 ] };
	key <KP7> { [ KP_Home, KP_7 ] };
	key <LVL3> { [ ISO_Level3_Shift ] };
	modifier_map Shift { <LFSH> };
	modifier_map Mod2 { <NMLK> };
	modifier_map Mod5 { <LVL3> };
};
};
"#;

    #[test]
    fn keymap_levels_groups_and_locks() {
        let map = Keymap::parse(SAMPLE);
        assert_eq!(map.key(9, 0, 0), Key::Sym(K_ESCAPE));
        assert_eq!(map.key(22, SHIFT, 0), Key::Sym(K_BACKSPACE));
        assert_eq!(map.key(10, 0, 0), Key::Char('&'));
        assert_eq!(map.key(10, SHIFT, 0), Key::Char('1'));
        assert_eq!(map.key(10, LEVEL3, 0), Key::Char('¹'));
        assert_eq!(map.key(24, 0, 0), Key::Char('a'));
        assert_eq!(map.key(24, SHIFT, 0), Key::Char('A'));
        assert_eq!(map.key(24, LOCK, 0), Key::Char('A'));
        assert_eq!(map.key(24, 0, 1), Key::Char('ф'));
        assert_eq!(map.key(24, CTRL, 1), Key::Char('a')); // shortcuts stay Latin
        assert_eq!(map.key(26, LEVEL3, 0), Key::Char('€'));
        assert_eq!(map.key(26, LOCK, 0), Key::Char('E'));
        assert_eq!(map.key(50, 0, 0), Key::None); // a modifier alone
        assert_eq!(map.key(79, 0, 0), Key::Sym(K_HOME));
        assert_eq!(map.key(79, NUM, 0), Key::Char('7'));
        assert_eq!(map.key(200, 0, 0), Key::None);
        // Symbols written as numbers.
        assert_eq!(map.key(25, 0, 0), Key::Char('z'));
        assert_eq!(map.key(25, LOCK, 0), Key::Char('Z'));
        assert_eq!(map.key(111, 0, 0), Key::Sym(K_UP));
        assert_eq!(map.key(38, SHIFT, 0), Key::Char('А'));
    }
}

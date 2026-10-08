//! Key chords: normalisation, parsing, display and resolution (spec §13,
//! README §A4, M1-06).
//!
//! A [`KeyChord`] is a key code plus the `CONTROL` / `ALT` / `SHIFT`
//! modifiers that matter. Terminals disagree on whether `G`, `$` or `?`
//! carry `SHIFT`, so it is dropped for characters: the character itself
//! already encodes it. Bindings are looked up by chord, never by raw
//! `KeyEvent`.
//!
//! Multi-key sequences are not supported (the spec has none): a binding such
//! as `"<g><g>"` is rejected with a warning when the config is loaded.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{action::Action, mode::KeyContext};

/// One key press, normalised. See the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct KeyChord {
    pub code: KeyCode,
    /// Only `CONTROL`, `ALT` and `SHIFT`.
    pub mods: KeyModifiers,
}

impl KeyChord {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        normalise(code, mods)
    }
}

impl From<KeyEvent> for KeyChord {
    fn from(key: KeyEvent) -> Self {
        normalise(key.code, key.modifiers)
    }
}

/// Whether `c` typed with `mods` is an AltGr character: `CONTROL | ALT` on
/// anything but an ASCII letter or digit. Windows terminals (and some Linux
/// layouts) report AltGr as Ctrl+Alt, and many layouts type `~ [ ] { } \ |
/// @` with it, so these are characters, not shortcuts. `ctrl-alt-a` stays a
/// shortcut.
pub fn is_altgr_char(c: char, mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) && !c.is_ascii_alphanumeric()
}

/// The normalisation rules of M1-06:
/// - only `CONTROL | ALT | SHIFT` are kept;
/// - an AltGr character ([`is_altgr_char`]) drops `CONTROL | ALT`;
/// - `Char` drops `SHIFT` (the character encodes it);
/// - `Char` with `CONTROL` is lowercased (`ctrl-D` ≡ `ctrl-d`);
/// - `BackTab` always has `SHIFT`.
fn normalise(code: KeyCode, mods: KeyModifiers) -> KeyChord {
    let mut mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
    let code = match code {
        KeyCode::Char(c) => {
            mods.remove(KeyModifiers::SHIFT);
            if is_altgr_char(c, mods) {
                mods.remove(KeyModifiers::CONTROL | KeyModifiers::ALT);
            }
            if mods.contains(KeyModifiers::CONTROL) {
                KeyCode::Char(c.to_ascii_lowercase())
            } else {
                KeyCode::Char(c)
            }
        }
        KeyCode::BackTab => {
            mods.insert(KeyModifiers::SHIFT);
            KeyCode::BackTab
        }
        other => other,
    };
    KeyChord { code, mods }
}

/// Parses one chord as written in the config.
///
/// - The template's `<…>` wrapper is optional: `"<ctrl-d>"` ≡ `"ctrl-d"`.
/// - Modifier prefixes (`ctrl-`, `alt-`, `shift-`) and named keys (`enter`,
///   `Esc`, `PageDown`) are case-insensitive.
/// - A single character keeps its case: `"G"` is `G`, `"g"` is `g`;
///   `"shift-g"` is also `G`.
/// - `<` and `>` delimit chords, so write them as `<lt>` and `<gt>` (a bare
///   `"<"` or `">"` is accepted too, being unambiguous).
/// - More than one chord (`"<g><g>"`) is an error: multi-key sequences are
///   not supported.
pub fn parse_key_chord(raw: &str) -> Result<KeyChord, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty key".to_owned());
    }
    let inner = if trimmed.chars().count() > 2 && trimmed.starts_with('<') && trimmed.ends_with('>')
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    if inner != trimmed && (inner == "<" || inner == ">") {
        return Err(format!("`{raw}` is ambiguous: write <lt> or <gt>"));
    }
    if inner.contains("><") {
        return Err(format!(
            "`{raw}`: multi-key sequences are not supported; bind a single key"
        ));
    }
    let (rest, mods) = split_modifiers(inner);
    let code = parse_code(rest, mods).ok_or_else(|| format!("unknown key `{raw}`"))?;
    Ok(KeyChord::new(code, mods))
}

/// Strips `ctrl-` / `alt-` / `shift-` prefixes (case-insensitive).
fn split_modifiers(raw: &str) -> (&str, KeyModifiers) {
    let mut mods = KeyModifiers::empty();
    let mut rest = raw;
    loop {
        let lower = rest.to_ascii_lowercase();
        let (len, m) = if lower.starts_with("ctrl-") && rest.len() > 5 {
            (5, KeyModifiers::CONTROL)
        } else if lower.starts_with("alt-") && rest.len() > 4 {
            (4, KeyModifiers::ALT)
        } else if lower.starts_with("shift-") && rest.len() > 6 {
            (6, KeyModifiers::SHIFT)
        } else {
            return (rest, mods);
        };
        mods.insert(m);
        rest = &rest[len..];
    }
}

fn parse_code(raw: &str, mods: KeyModifiers) -> Option<KeyCode> {
    let mut chars = raw.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        // A single character keeps its case; `shift-` uppercases it.
        return Some(KeyCode::Char(if mods.contains(KeyModifiers::SHIFT) {
            c.to_ascii_uppercase()
        } else {
            c
        }));
    }
    let lower = raw.to_ascii_lowercase();
    Some(match lower.as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "backtab" => KeyCode::BackTab,
        "tab" if mods.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        "hyphen" | "minus" => KeyCode::Char('-'),
        "lt" => KeyCode::Char('<'),
        "gt" => KeyCode::Char('>'),
        f if f.starts_with('f') => {
            let n: u8 = f[1..].parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            KeyCode::F(n)
        }
        _ => return None,
    })
}

/// How a chord is shown in the hint line, help and palette: `G`, `ctrl-d`,
/// `PgDn`, `←`, `Enter`, `shift-Tab`.
pub fn chord_to_string(chord: KeyChord) -> String {
    let mut out = String::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        out.push_str("ctrl-");
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        out.push_str("alt-");
    }
    // `SHIFT` is part of the character for `Char`; `BackTab` reads `shift-Tab`.
    if chord.mods.contains(KeyModifiers::SHIFT) {
        out.push_str("shift-");
    }
    let key = match chord.code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::Tab | KeyCode::BackTab => "Tab".to_owned(),
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Delete => "Del".to_owned(),
        KeyCode::Insert => "Ins".to_owned(),
        KeyCode::Left => "←".to_owned(),
        KeyCode::Right => "→".to_owned(),
        KeyCode::Up => "↑".to_owned(),
        KeyCode::Down => "↓".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PgUp".to_owned(),
        KeyCode::PageDown => "PgDn".to_owned(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    out.push_str(&key);
    out
}

/// Key bindings per [`KeyContext`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keymap(pub HashMap<KeyContext, HashMap<KeyChord, Action>>);

impl Keymap {
    /// The action bound to `chord` in `context`, if any.
    pub fn resolve(&self, context: KeyContext, chord: KeyChord) -> Option<&Action> {
        self.0.get(&context)?.get(&chord)
    }

    /// The chord to show for `action` in `context`: of all chords bound to
    /// it, the one without modifiers, then the shortest, then the first in
    /// text order. Deterministic, although bindings live in a `HashMap`.
    pub fn display_chord(&self, context: KeyContext, action: &Action) -> Option<String> {
        self.0
            .get(&context)?
            .iter()
            .filter(|(_, a)| *a == action)
            .map(|(chord, _)| (!chord.mods.is_empty(), chord_to_string(*chord)))
            .min_by(|(ma, a), (mb, b)| {
                ma.cmp(mb)
                    .then(a.chars().count().cmp(&b.chars().count()))
                    .then(a.cmp(b))
            })
            .map(|(_, s)| s)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};
    use pretty_assertions::assert_eq;

    use super::*;

    fn chord(code: KeyCode, mods: KeyModifiers) -> KeyChord {
        KeyChord { code, mods }
    }

    fn ch(c: char) -> KeyChord {
        chord(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn single_characters_keep_their_case() {
        assert_eq!(parse_key_chord("G").unwrap(), ch('G'));
        assert_eq!(parse_key_chord("g").unwrap(), ch('g'));
        assert_eq!(parse_key_chord("shift-g").unwrap(), ch('G'));
        assert_eq!(parse_key_chord("<shift-g>").unwrap(), ch('G'));
        assert_eq!(parse_key_chord("N").unwrap(), ch('N'));
        assert_eq!(parse_key_chord("$").unwrap(), ch('$'));
        assert_eq!(parse_key_chord("?").unwrap(), ch('?'));
        assert_eq!(parse_key_chord(":").unwrap(), ch(':'));
        assert_ne!(parse_key_chord("A").unwrap(), parse_key_chord("a").unwrap());
    }

    #[test]
    fn lt_and_gt() {
        assert_eq!(parse_key_chord("<lt>").unwrap(), ch('<'));
        assert_eq!(parse_key_chord("<gt>").unwrap(), ch('>'));
        assert_eq!(parse_key_chord("LT").unwrap(), ch('<'));
        assert_eq!(parse_key_chord("<").unwrap(), ch('<'));
        assert_eq!(parse_key_chord(">").unwrap(), ch('>'));
        assert!(parse_key_chord("<<>").unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn modifiers_and_named_keys_are_case_insensitive() {
        let ctrl_a = chord(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(parse_key_chord("CTRL-a").unwrap(), ctrl_a);
        assert_eq!(parse_key_chord("<Ctrl-A>").unwrap(), ctrl_a);
        assert_eq!(parse_key_chord("ctrl-a").unwrap(), ctrl_a);
        assert_eq!(
            parse_key_chord("AlT-eNtEr").unwrap(),
            chord(KeyCode::Enter, KeyModifiers::ALT)
        );
        assert_eq!(
            parse_key_chord("<Esc>").unwrap(),
            chord(KeyCode::Esc, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key_chord("PageDown").unwrap(),
            chord(KeyCode::PageDown, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key_chord("pgdn").unwrap(),
            parse_key_chord("pagedown").unwrap()
        );
        assert_eq!(
            parse_key_chord("<pgup>").unwrap(),
            chord(KeyCode::PageUp, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key_chord("ctrl-alt-a").unwrap(),
            chord(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )
        );
        assert_eq!(
            parse_key_chord("ctrl-shift-enter").unwrap(),
            chord(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        );
        assert_eq!(
            parse_key_chord("shift-tab").unwrap(),
            chord(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
        assert_eq!(
            parse_key_chord("backtab").unwrap(),
            chord(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
        assert_eq!(parse_key_chord("space").unwrap(), ch(' '));
        assert_eq!(parse_key_chord("minus").unwrap(), ch('-'));
        assert_eq!(
            parse_key_chord("F5").unwrap(),
            chord(KeyCode::F(5), KeyModifiers::NONE)
        );
    }

    #[test]
    fn invalid_keys() {
        assert!(parse_key_chord("").is_err());
        assert!(parse_key_chord("invalid-key").is_err());
        assert!(parse_key_chord("ctrl-invalid-key").is_err());
        assert!(parse_key_chord("f13").is_err());
        assert!(parse_key_chord("<g><g>").unwrap_err().contains("multi-key"));
        assert!(parse_key_chord("<ctrl-x><ctrl-c>").is_err());
    }

    #[test]
    fn altgr_characters_normalise_to_the_plain_character() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(KeyChord::new(KeyCode::Char('~'), altgr), ch('~'));
        assert_eq!(
            KeyChord::new(KeyCode::Char('{'), altgr | KeyModifiers::SHIFT),
            ch('{')
        );
        // Letters and digits keep the shortcut.
        assert_eq!(
            KeyChord::new(KeyCode::Char('a'), altgr),
            chord(KeyCode::Char('a'), altgr)
        );
        assert!(!is_altgr_char('7', altgr));
        assert!(!is_altgr_char('~', KeyModifiers::ALT));
        assert!(!is_altgr_char('~', KeyModifiers::CONTROL));
    }

    #[test]
    fn shift_is_dropped_for_characters() {
        let shifted = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(KeyChord::from(shifted), ch('G'));
        let plain = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE);
        assert_eq!(KeyChord::from(plain), ch('G'));
        let dollar = KeyEvent::new(KeyCode::Char('$'), KeyModifiers::SHIFT);
        assert_eq!(KeyChord::from(dollar), ch('$'));
        // `ctrl-D` ≡ `ctrl-d`.
        let ctrl = KeyEvent::new(
            KeyCode::Char('D'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(
            KeyChord::from(ctrl),
            chord(KeyCode::Char('d'), KeyModifiers::CONTROL)
        );
        // BackTab keeps (or gains) SHIFT.
        assert_eq!(
            KeyChord::from(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
            chord(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
        // Other modifiers, `kind` and `state` are stripped.
        let mut repeat = KeyEvent::new(KeyCode::Down, KeyModifiers::SUPER);
        repeat.kind = KeyEventKind::Repeat;
        repeat.state = KeyEventState::CAPS_LOCK;
        assert_eq!(
            KeyChord::from(repeat),
            chord(KeyCode::Down, KeyModifiers::NONE)
        );
    }

    #[test]
    fn display() {
        let show = |s: &str| chord_to_string(parse_key_chord(s).unwrap());
        assert_eq!(show("G"), "G");
        assert_eq!(show("<ctrl-d>"), "ctrl-d");
        assert_eq!(show("pgdn"), "PgDn");
        assert_eq!(show("left"), "←");
        assert_eq!(show("enter"), "Enter");
        assert_eq!(show("esc"), "Esc");
        assert_eq!(show("shift-tab"), "shift-Tab");
        assert_eq!(show("alt-enter"), "alt-Enter");
        assert_eq!(show("<lt>"), "<");
    }

    #[test]
    fn display_chord_prefers_plain_short_keys() {
        let mut normal = HashMap::new();
        normal.insert(parse_key_chord("ctrl-c").unwrap(), Action::Quit);
        normal.insert(parse_key_chord("ctrl-q").unwrap(), Action::Quit);
        normal.insert(parse_key_chord("q").unwrap(), Action::Quit);
        normal.insert(parse_key_chord("j").unwrap(), Action::MoveDown);
        normal.insert(parse_key_chord("down").unwrap(), Action::MoveDown);
        let keymap = Keymap(HashMap::from([(KeyContext::Normal, normal)]));
        assert_eq!(
            keymap.display_chord(KeyContext::Normal, &Action::Quit),
            Some("q".to_owned())
        );
        assert_eq!(
            keymap.display_chord(KeyContext::Normal, &Action::MoveDown),
            Some("j".to_owned())
        );
        assert_eq!(
            keymap.display_chord(KeyContext::Normal, &Action::Help),
            None
        );
        assert_eq!(
            keymap.display_chord(KeyContext::Filter, &Action::Quit),
            None
        );
        assert_eq!(
            keymap.resolve(KeyContext::Normal, ch('q')),
            Some(&Action::Quit)
        );
        assert_eq!(keymap.resolve(KeyContext::Normal, ch('x')), None);
    }
}

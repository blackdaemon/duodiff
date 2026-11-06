//! Building a [`Keymap`] from the `[keys]` config section (Issue #339).
//!
//! Each entry names a Command and the keys that replace its defaults on every
//! screen that binds it. An entry that cannot apply as written is ignored as a
//! whole, the Command keeps its defaults, and the reason is reported.

use super::{Chord, Gesture, KeyAction, Keymap, MotionBinding};
use crate::app::ViewMode;
use crate::commands::Command;
use crossterm::event::{KeyCode, KeyModifiers};

const SCREENS: [ViewMode; 4] = [
    ViewMode::DirectoryTree,
    ViewMode::FileDiff,
    ViewMode::ConfigMenu,
    ViewMode::Help,
];

fn screen_name(screen: ViewMode) -> &'static str {
    match screen {
        ViewMode::DirectoryTree => "Directory Tree",
        ViewMode::FileDiff => "File Diff",
        ViewMode::ConfigMenu => "Config",
        ViewMode::Help => "Help",
    }
}

/// What a `[keys]` entry binds: a Command, or a motion gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Command(Command),
    Motion(Gesture),
}

impl Target {
    fn from_config_name(name: &str) -> Option<Self> {
        Command::from_config_name(name)
            .map(Target::Command)
            .or_else(|| super::gestures::motion_from_config_name(name).map(Target::Motion))
    }

    fn of(action: KeyAction) -> Self {
        match action {
            KeyAction::Command(command) => Target::Command(command),
            KeyAction::Gesture(gesture) => Target::Motion(gesture),
        }
    }

    fn config_name(self) -> Option<&'static str> {
        match self {
            Target::Command(command) => command.config_name(),
            Target::Motion(gesture) => super::gestures::motion_config_name(gesture),
        }
    }
}

/// One `[keys]` entry that passed its own checks.
struct Entry {
    target: Target,
    name: String,
    chords: Vec<Chord>,
}

impl Keymap {
    /// The default keymap with the `[keys]` entries in `keys` applied, and one
    /// sentence per entry that was ignored.
    pub fn with_overrides(keys: &toml::Table) -> (Keymap, Vec<String>) {
        let defaults = Keymap::default();
        let mut problems = Vec::new();
        let mut entries = Vec::new();
        for (name, value) in keys {
            match parse_entry(&defaults, name, value) {
                Ok(entry) => entries.push(entry),
                Err(reason) => problems.push(format!("keys.{name}: {reason}")),
            }
        }

        // Dropping an entry gives its Command back its defaults, which can
        // collide with another entry in turn, so resolve until nothing does.
        loop {
            let keymap = apply(&defaults, &entries);
            let rejected = collisions(&keymap, &entries);
            if rejected.is_empty() {
                // By entry name, the order the section itself reads in.
                problems.sort();
                return (keymap, problems);
            }
            for (index, reason) in rejected.iter().rev() {
                let entry = entries.remove(*index);
                problems.push(format!("keys.{}: {reason}", entry.name));
            }
        }
    }
}

fn parse_entry(defaults: &Keymap, name: &str, value: &toml::Value) -> Result<Entry, String> {
    let target = Target::from_config_name(name)
        .ok_or_else(|| format!("no command or motion is named `{name}`"))?;
    let specs: Vec<&str> = match value {
        toml::Value::String(spec) => vec![spec.as_str()],
        toml::Value::Array(items) => items
            .iter()
            .map(|item| item.as_str())
            .collect::<Option<_>>()
            .ok_or("expected a key or a list of keys")?,
        _ => return Err("expected a key or a list of keys".to_string()),
    };
    let mut chords: Vec<Chord> = Vec::new();
    for spec in specs {
        let chord = parse_sequence(spec)?;
        if !chords.iter().any(|c| same_chord(c, &chord)) {
            chords.push(chord);
        }
    }
    if let Some(first) = chords.iter().find_map(|chord| {
        chord
            .prefix_chord()
            .filter(|first| chords.iter().any(|other| same_chord(other, first)))
    }) {
        return Err(format!(
            "`{}` cannot both be a key and start a sequence",
            super::format_chord(&first)
        ));
    }
    let screens = match target {
        Target::Command(command) => screens_binding(defaults, command),
        Target::Motion(_) => super::gestures::MOTION_SCREENS.to_vec(),
    };
    for screen in screens {
        // A sequence is reachable as long as its first key is.
        if let Some(chord) = chords
            .iter()
            .map(|chord| chord.prefix_chord().unwrap_or(*chord))
            .find(|chord| reserved_on(screen, chord))
        {
            return Err(format!(
                "`{}` is handled by {} itself and cannot be bound",
                super::format_chord(&chord),
                screen_name(screen)
            ));
        }
    }
    Ok(Entry {
        target,
        name: name.to_string(),
        chords,
    })
}

/// Parse one entry key: a single key, or a two-key sequence written as the
/// two keys apart (`"g g"`). A lone `" "` is the space bar, not a separator.
fn parse_sequence(spec: &str) -> Result<Chord, String> {
    let keys: Vec<&str> = if spec.trim().is_empty() {
        vec![spec]
    } else {
        spec.split_whitespace().collect()
    };
    match keys.as_slice() {
        [key] => parse_chord(key),
        [first, second] => {
            let first = parse_chord(first)?;
            let second = parse_chord(second)?;
            Ok(Chord {
                prefix: Some((first.code, first.modifiers)),
                ..second
            })
        }
        _ => Err(format!("`{spec}`: a sequence is at most two keys")),
    }
}

/// Parse one key: a single character taken literally (`"L"` is not `"l"`), or
/// a named key, optionally after `ctrl+` and `alt+`.
pub(crate) fn parse_chord(spec: &str) -> Result<Chord, String> {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = spec;
    loop {
        let lower = rest.to_ascii_lowercase();
        if rest.chars().count() > 1 && lower.starts_with("ctrl+") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[5..];
        } else if rest.chars().count() > 1 && lower.starts_with("alt+") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[4..];
        } else if rest.chars().count() > 1 && lower.starts_with("shift+") {
            return Err(format!(
                "`{spec}`: write an uppercase letter or the shifted symbol instead of `shift+`"
            ));
        } else {
            break;
        }
    }
    let mut chars = rest.chars();
    let code = match (chars.next(), chars.next()) {
        (Some(c), None) if modifiers.contains(KeyModifiers::CONTROL) => {
            KeyCode::Char(c.to_ascii_lowercase())
        }
        (Some(c), None) => KeyCode::Char(c),
        _ => named_key(rest).ok_or_else(|| format!("`{spec}` is not a key"))?,
    };
    Ok(Chord {
        code,
        modifiers,
        shown: true,
        prefix: None,
    })
}

fn named_key(name: &str) -> Option<KeyCode> {
    let name = name.to_ascii_lowercase();
    Some(match name.as_str() {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        _ => {
            let n: u8 = name.strip_prefix('f')?.parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            KeyCode::F(n)
        }
    })
}

fn same_chord(a: &Chord, b: &Chord) -> bool {
    a.code == b.code && a.modifiers == b.modifiers && a.prefix == b.prefix
}

/// Whether `a` is the first key of `b`'s sequence, so `a` would never run.
fn starts(a: &Chord, b: &Chord) -> bool {
    b.prefix_chord().is_some_and(|first| same_chord(a, &first))
}

/// The screens whose defaults bind `command`; every screen for the global one.
fn screens_binding(defaults: &Keymap, command: Command) -> Vec<ViewMode> {
    if defaults.global.iter().any(|b| b.command == command) {
        return SCREENS.to_vec();
    }
    SCREENS
        .into_iter()
        .filter(|screen| {
            defaults
                .screen_bindings(*screen)
                .iter()
                .any(|b| b.command == command)
        })
        .collect()
}

/// Whether the input adapter answers `chord` on `screen` before any binding
/// is consulted — a [`super::Gesture`] — so a Command bound to it could never
/// run. Read from the same table dispatch reads.
pub(crate) fn reserved_on(screen: ViewMode, chord: &Chord) -> bool {
    super::gestures::gesture_on(screen, chord.code, chord.modifiers).is_some()
}

fn apply(defaults: &Keymap, entries: &[Entry]) -> Keymap {
    let mut keymap = defaults.clone();
    for entry in entries {
        let command = match entry.target {
            Target::Command(command) => command,
            Target::Motion(gesture) => {
                keymap.motions.push(MotionBinding {
                    gesture,
                    chords: entry.chords.clone(),
                });
                continue;
            }
        };
        for table in [
            &mut keymap.global,
            &mut keymap.directory_tree,
            &mut keymap.file_diff,
            &mut keymap.config_menu,
            &mut keymap.help,
        ] {
            for binding in table.iter_mut().filter(|b| b.command == command) {
                binding.chords = entry.chords.clone();
            }
        }
        keymap.customized.push(command);
    }
    keymap
}

/// Entries to drop, by index, with the reason: a key that already runs
/// another Command or motion on the same screen, or that starts another's
/// sequence. Against a default, only the entry goes; between two entries,
/// both do.
fn collisions(keymap: &Keymap, entries: &[Entry]) -> Vec<(usize, String)> {
    let index_of = |target: Target| entries.iter().position(|e| e.target == target);
    let mut rejected: Vec<(usize, String)> = Vec::new();
    for screen in SCREENS {
        let table: Vec<(Target, &[Chord])> = keymap
            .targets_on(screen)
            .map(|(action, chords)| (Target::of(action), chords))
            .collect();
        for (i, (a, a_chords)) in table.iter().enumerate() {
            for (b, b_chords) in &table[i + 1..] {
                if a == b {
                    continue;
                }
                let Some((ca, cb)) = a_chords.iter().find_map(|ca| {
                    b_chords
                        .iter()
                        .find(|cb| same_chord(ca, cb) || starts(ca, cb) || starts(cb, ca))
                        .map(|cb| (ca, cb))
                }) else {
                    continue;
                };
                for (this, mine, other, theirs) in [(a, ca, b, cb), (b, cb, a, ca)] {
                    let Some(index) = index_of(*this) else {
                        continue;
                    };
                    if rejected.iter().any(|(i, _)| *i == index) {
                        continue;
                    }
                    let other_name = other.config_name().unwrap_or("another command");
                    let reason = if same_chord(mine, theirs) {
                        format!(
                            "`{}` already runs `{other_name}`",
                            super::format_chord(mine)
                        )
                    } else if starts(mine, theirs) {
                        format!(
                            "`{}` starts `{}`, which runs `{other_name}`",
                            super::format_chord(mine),
                            super::format_chord(theirs)
                        )
                    } else {
                        format!(
                            "`{}` starts with `{}`, which already runs `{other_name}`",
                            super::format_chord(mine),
                            super::format_chord(theirs)
                        )
                    };
                    rejected.push((index, format!("{reason} on {}", screen_name(screen))));
                }
            }
        }
    }
    rejected.sort_by_key(|(index, _)| *index);
    rejected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(toml_text: &str) -> toml::Table {
        toml_text.parse::<toml::Table>().unwrap()
    }

    fn remap(toml_text: &str) -> (Keymap, Vec<String>) {
        Keymap::with_overrides(&keys(toml_text))
    }

    fn press(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// The issue's own example: swap the copy keys on every screen that has them.
    #[test]
    fn swapping_the_copy_keys_applies_on_every_screen() {
        let (keymap, problems) = remap("copy_to_left = \"R\"\ncopy_to_right = \"L\"\n");

        assert_eq!(problems, Vec::<String>::new());
        for screen in [ViewMode::DirectoryTree, ViewMode::FileDiff] {
            assert_eq!(
                keymap.command_for_key(screen, &press('R')),
                Some(Command::CopyRightToLeft)
            );
            assert_eq!(
                keymap.command_for_key(screen, &press('L')),
                Some(Command::CopyLeftToRight)
            );
        }
        assert!(keymap.is_customized(Command::CopyRightToLeft));
        assert!(!keymap.is_customized(Command::Refresh));
        assert_eq!(keymap.customized_count(), 2);
    }

    /// Listed keys replace the defaults and are all shown; `[]` unbinds.
    #[test]
    fn entries_replace_the_defaults_and_an_empty_list_unbinds() {
        let (keymap, problems) =
            remap("next_change = [\"n\", \"alt+down\", \"Ctrl+X\"]\nsave_staged = []\n");

        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(keymap.hint(Command::NextChange), "n / Alt+Down / Ctrl+x");
        assert_eq!(
            keymap.command_for_key(ViewMode::FileDiff, &press('N')),
            None
        );
        assert_eq!(keymap.key_phrase(Command::SaveStaged), None);
        assert_eq!(
            keymap.command_for_key(ViewMode::FileDiff, &press('s')),
            None
        );
    }

    #[test]
    fn key_specs_parse_literally_with_named_keys_and_modifiers() {
        let chord = |spec| parse_chord(spec).map(|c| (c.code, c.modifiers));
        assert_eq!(chord("L"), Ok((KeyCode::Char('L'), KeyModifiers::NONE)));
        assert_eq!(chord("l"), Ok((KeyCode::Char('l'), KeyModifiers::NONE)));
        assert_eq!(chord("+"), Ok((KeyCode::Char('+'), KeyModifiers::NONE)));
        assert_eq!(chord("alt++"), Ok((KeyCode::Char('+'), KeyModifiers::ALT)));
        assert_eq!(
            chord("PageDown"),
            Ok((KeyCode::PageDown, KeyModifiers::NONE))
        );
        assert_eq!(chord("f12"), Ok((KeyCode::F(12), KeyModifiers::NONE)));
        assert_eq!(
            chord("ctrl+alt+enter"),
            Ok((KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::ALT))
        );
        for bad in ["", "ctrl+", "f13", "enterr", "shift+x"] {
            assert!(chord(bad).is_err(), "{bad:?} should not parse");
        }
        assert!(chord("shift+x").unwrap_err().contains("uppercase"));
    }

    /// An entry that cannot apply as written is ignored whole, with the reason;
    /// the Command keeps its defaults and the rest of the section still applies.
    #[test]
    fn a_broken_entry_is_reported_and_leaves_the_defaults_alone() {
        let (keymap, problems) = remap(
            "bogus = \"x\"\nrescan = 5\nfilter = [\"/\", 3]\nquit = \"shift+q\"\n\
             help = \"j\"\nswitch_theme = \"k\"\nexternal_edit = \"e\"\n",
        );

        assert_eq!(
            problems,
            [
                "keys.bogus: no command or motion is named `bogus`",
                "keys.filter: expected a key or a list of keys",
                "keys.help: `j` is handled by Directory Tree itself and cannot be bound",
                "keys.quit: `shift+q`: write an uppercase letter or the shifted symbol instead of `shift+`",
                "keys.rescan: expected a key or a list of keys",
                "keys.switch_theme: `k` is handled by Directory Tree itself and cannot be bound",
            ]
        );
        assert_eq!(keymap.hint(Command::Refresh), "r");
        assert_eq!(keymap.hint(Command::Help), "?");
        assert_eq!(keymap.hint(Command::ExternalEdit), "e");
    }

    /// A key another Command already answers on the same screen: against a
    /// default only the entry goes; between two entries both do; and a Command
    /// handed its defaults back can push out an entry that took them.
    #[test]
    fn colliding_entries_are_dropped_until_nothing_collides() {
        let (keymap, problems) = remap("copy_to_right = \"r\"\n");
        assert_eq!(
            problems,
            ["keys.copy_to_right: `r` already runs `rescan` on Directory Tree"]
        );
        assert_eq!(keymap.hint(Command::CopyLeftToRight), "R");

        let (keymap, problems) = remap("rescan = \"x\"\nfilter = \"x\"\n");
        assert_eq!(
            problems,
            [
                "keys.filter: `x` already runs `rescan` on Directory Tree",
                "keys.rescan: `x` already runs `filter` on Directory Tree",
            ]
        );
        assert_eq!(keymap.hint(Command::Refresh), "r");
        assert_eq!(keymap.hint(Command::Filter), "/");

        let (keymap, problems) = remap("copy_to_right = \"L\"\ncopy_to_left = \"j\"\n");
        assert_eq!(
            problems,
            [
                "keys.copy_to_left: `j` is handled by Directory Tree itself and cannot be bound",
                "keys.copy_to_right: `L` already runs `copy_to_left` on Directory Tree",
            ]
        );
        assert_eq!(keymap.hint(Command::CopyLeftToRight), "R");
        assert_eq!(keymap.hint(Command::CopyRightToLeft), "L");
        assert_eq!(keymap.customized_count(), 0);
    }

    /// Only keys the adapter takes unconditionally are refused: `Enter`, `Esc`
    /// and `Backspace` still reach the Directory Tree's bindings in other states.
    #[test]
    fn conditionally_handled_keys_stay_bindable() {
        let (keymap, problems) = remap("rescan = \"backspace\"\nhelp = [\"?\", \"F1\"]\n");
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(keymap.hint(Command::Refresh), "Backspace");
        assert_eq!(keymap.hint(Command::Help), "? / F1");
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, modifiers)
    }

    /// Motions have no default keys; `[keys]` binds them on the Directory Tree
    /// and File Diff, and nowhere else.
    #[test]
    fn motions_bind_on_the_tree_and_the_diff() {
        let (keymap, problems) = remap(
            "first_row = \"home\"\nlast_row = [\"G\", \"end\"]\nhalf_page_down = \"ctrl+d\"\npage_down = \"pagedown\"\n",
        );
        assert_eq!(problems, Vec::<String>::new());
        for screen in [ViewMode::DirectoryTree, ViewMode::FileDiff] {
            assert_eq!(
                keymap.action_for_key(screen, &key(KeyCode::Home, KeyModifiers::NONE)),
                Some(KeyAction::Gesture(Gesture::Top))
            );
            assert_eq!(
                keymap.action_for_key(screen, &press('G')),
                Some(KeyAction::Gesture(Gesture::Bottom))
            );
            assert_eq!(
                keymap.action_for_key(screen, &key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
                Some(KeyAction::Gesture(Gesture::HalfPageDown))
            );
            assert_eq!(
                keymap.action_for_key(screen, &key(KeyCode::PageDown, KeyModifiers::NONE)),
                Some(KeyAction::Gesture(Gesture::PageDown))
            );
        }
        assert_eq!(
            keymap.action_for_key(ViewMode::Help, &press('G')),
            None,
            "motions do not reach Help"
        );
        assert_eq!(
            keymap.motion_hint(Gesture::Bottom),
            Some("G / End".to_string())
        );
        assert_eq!(keymap.motion_hint(Gesture::HalfPageUp), None);
        assert_eq!(
            Keymap::default().action_for_key(ViewMode::DirectoryTree, &press('G')),
            None,
            "a motion has no key until `[keys]` gives it one"
        );
    }

    /// `"g g"` is a two-key sequence: the first `g` only starts it, and the
    /// pair resolves to the motion. Commands take sequences the same way.
    #[test]
    fn a_two_key_sequence_binds_a_motion_or_a_command() {
        let (keymap, problems) = remap("first_row = \"g g\"\nrescan = \"g r\"\n");
        assert_eq!(problems, Vec::<String>::new());
        let tree = ViewMode::DirectoryTree;
        assert!(keymap.starts_sequence(tree, &press('g')));
        assert_eq!(keymap.action_for_key(tree, &press('g')), None);
        assert_eq!(
            keymap.action_after(tree, &press('g'), &press('g')),
            Some(KeyAction::Gesture(Gesture::Top))
        );
        assert_eq!(
            keymap.action_after(tree, &press('g'), &press('r')),
            Some(KeyAction::Command(Command::Refresh))
        );
        assert_eq!(keymap.action_after(tree, &press('g'), &press('x')), None);
        assert_eq!(keymap.motion_hint(Gesture::Top), Some("g g".to_string()));
        assert_eq!(keymap.hint(Command::Refresh), "g r");
        // `r` alone no longer rescans; it is not a sequence's first key either.
        assert_eq!(keymap.action_for_key(tree, &press('r')), None);
        assert!(!keymap.starts_sequence(tree, &press('r')));
    }

    /// A sequence's first key cannot also run something on its own, or one of
    /// the two could never run.
    #[test]
    fn a_sequence_cannot_start_with_a_bound_key() {
        let (keymap, problems) = remap("first_row = \"s s\"\n");
        assert_eq!(
            problems,
            ["keys.first_row: `s s` starts with `s`, which already runs `swap_sides` on Directory Tree"]
        );
        assert_eq!(keymap.motion_hint(Gesture::Top), None);

        let (_, problems) = remap("first_row = [\"g\", \"g g\"]\n");
        assert_eq!(
            problems,
            ["keys.first_row: `g` cannot both be a key and start a sequence"]
        );

        let (_, problems) = remap("first_row = \"j j\"\n");
        assert_eq!(
            problems,
            ["keys.first_row: `j` is handled by Directory Tree itself and cannot be bound"]
        );

        let (_, problems) = remap("first_row = \"g g g\"\n");
        assert_eq!(
            problems,
            ["keys.first_row: `g g g`: a sequence is at most two keys"]
        );

        // The space bar on its own is still a key, not an empty sequence.
        let (_, problems) = remap("rescan = \" \"\n");
        assert_eq!(
            problems,
            ["keys.rescan: `Space` is handled by Directory Tree itself and cannot be bound"]
        );
    }

    /// Two entries that clash through a sequence are both dropped.
    #[test]
    fn a_key_and_a_sequence_it_starts_reject_each_other() {
        let (keymap, problems) = remap("first_row = \"g g\"\nlast_row = \"g\"\n");
        assert_eq!(
            problems,
            [
                "keys.first_row: `g g` starts with `g`, which already runs `last_row` on Directory Tree",
                "keys.last_row: `g` starts `g g`, which runs `first_row` on Directory Tree",
            ]
        );
        assert!(keymap.motions.is_empty());
    }
}

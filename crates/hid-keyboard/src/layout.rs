//! Usage-to-key tables (HID Usage Tables, Keyboard/Keypad page 07h).

use crate::{Key, Named};

/// Keyboard layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layout {
    /// US QWERTY.
    Us,
    /// Russian ЙЦУКЕН on the same physical keys.
    Russian,
}

/// Unshifted and shifted characters of the main block, usages 04h..38h.
const US: [(char, char); 0x35] = [
    ('a', 'A'),
    ('b', 'B'),
    ('c', 'C'),
    ('d', 'D'),
    ('e', 'E'),
    ('f', 'F'),
    ('g', 'G'),
    ('h', 'H'),
    ('i', 'I'),
    ('j', 'J'),
    ('k', 'K'),
    ('l', 'L'),
    ('m', 'M'),
    ('n', 'N'),
    ('o', 'O'),
    ('p', 'P'),
    ('q', 'Q'),
    ('r', 'R'),
    ('s', 'S'),
    ('t', 'T'),
    ('u', 'U'),
    ('v', 'V'),
    ('w', 'W'),
    ('x', 'X'),
    ('y', 'Y'),
    ('z', 'Z'), // 04h..1Dh
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('4', '$'),
    ('5', '%'),
    ('6', '^'),
    ('7', '&'),
    ('8', '*'),
    ('9', '('),
    ('0', ')'), // 1Eh..27h
    ('\0', '\0'),
    ('\0', '\0'),
    ('\0', '\0'),
    ('\0', '\0'), // 28h..2Bh named
    (' ', ' '),
    ('-', '_'),
    ('=', '+'),
    ('[', '{'),
    (']', '}'),
    ('\\', '|'),
    ('#', '~'),
    (';', ':'),
    ('\'', '"'),
    ('`', '~'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'), // 2Ch..38h
];

/// The same keys in the Russian layout.
const RU: [(char, char); 0x35] = [
    ('ф', 'Ф'),
    ('и', 'И'),
    ('с', 'С'),
    ('в', 'В'),
    ('у', 'У'),
    ('а', 'А'),
    ('п', 'П'),
    ('р', 'Р'),
    ('ш', 'Ш'),
    ('о', 'О'),
    ('л', 'Л'),
    ('д', 'Д'),
    ('ь', 'Ь'),
    ('т', 'Т'),
    ('щ', 'Щ'),
    ('з', 'З'),
    ('й', 'Й'),
    ('к', 'К'),
    ('ы', 'Ы'),
    ('е', 'Е'),
    ('г', 'Г'),
    ('м', 'М'),
    ('ц', 'Ц'),
    ('ч', 'Ч'),
    ('н', 'Н'),
    ('я', 'Я'), // 04h..1Dh
    ('1', '!'),
    ('2', '"'),
    ('3', '№'),
    ('4', ';'),
    ('5', '%'),
    ('6', ':'),
    ('7', '?'),
    ('8', '*'),
    ('9', '('),
    ('0', ')'), // 1Eh..27h
    ('\0', '\0'),
    ('\0', '\0'),
    ('\0', '\0'),
    ('\0', '\0'), // 28h..2Bh named
    (' ', ' '),
    ('-', '_'),
    ('=', '+'),
    ('х', 'Х'),
    ('ъ', 'Ъ'),
    ('\\', '/'),
    ('\\', '/'),
    ('ж', 'Ж'),
    ('э', 'Э'),
    ('ё', 'Ё'),
    ('б', 'Б'),
    ('ю', 'Ю'),
    ('.', ','), // 2Ch..38h
];

/// Resolves a usage to a key for the given layout and state.
pub(crate) fn resolve(layout: Layout, usage: u8, shift: bool, caps: bool, num: bool) -> Key {
    match usage {
        0x28 => Key::Named(Named::Enter),
        0x29 => Key::Named(Named::Escape),
        0x2A => Key::Named(Named::Backspace),
        0x2B => Key::Named(Named::Tab),
        0x04..=0x38 => {
            let table = match layout {
                Layout::Us => &US,
                Layout::Russian => &RU,
            };
            let (lower, upper) = table[usize::from(usage - 0x04)];
            // Caps Lock inverts Shift for letters only.
            let letter = lower.is_alphabetic();
            let up = if letter { shift != caps } else { shift };
            Key::Char(if up { upper } else { lower })
        }
        0x39 => Key::Named(Named::CapsLock),
        0x3A..=0x45 => Key::Named(Named::F(usage - 0x39)),
        0x46 => Key::Named(Named::PrintScreen),
        0x47 => Key::Named(Named::ScrollLock),
        0x48 => Key::Named(Named::Pause),
        0x49 => Key::Named(Named::Insert),
        0x4A => Key::Named(Named::Home),
        0x4B => Key::Named(Named::PageUp),
        0x4C => Key::Named(Named::Delete),
        0x4D => Key::Named(Named::End),
        0x4E => Key::Named(Named::PageDown),
        0x4F => Key::Named(Named::Right),
        0x50 => Key::Named(Named::Left),
        0x51 => Key::Named(Named::Down),
        0x52 => Key::Named(Named::Up),
        0x53 => Key::Named(Named::NumLock),
        0x54 => Key::Char('/'),
        0x55 => Key::Char('*'),
        0x56 => Key::Char('-'),
        0x57 => Key::Char('+'),
        0x58 => Key::Named(Named::Enter),
        0x59..=0x63 => keypad(usage, num && !shift),
        0x64 => Key::Char(match (layout, shift) {
            (Layout::Us, false) => '\\',
            (Layout::Us, true) => '|',
            (Layout::Russian, false) => '\\',
            (Layout::Russian, true) => '/',
        }),
        0x65 => Key::Named(Named::Application),
        0xE0 => Key::Named(Named::LeftCtrl),
        0xE1 => Key::Named(Named::LeftShift),
        0xE2 => Key::Named(Named::LeftAlt),
        0xE3 => Key::Named(Named::LeftGui),
        0xE4 => Key::Named(Named::RightCtrl),
        0xE5 => Key::Named(Named::RightShift),
        0xE6 => Key::Named(Named::RightAlt),
        0xE7 => Key::Named(Named::RightGui),
        _ => Key::Unknown(usage),
    }
}

/// Keypad 1..9, 0 and '.' (usages 59h..63h): digits with Num Lock (and no
/// Shift), navigation otherwise.
fn keypad(usage: u8, digits: bool) -> Key {
    const DIGITS: [char; 11] = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0', '.'];
    const NAV: [Option<Named>; 11] = [
        Some(Named::End),
        Some(Named::Down),
        Some(Named::PageDown),
        Some(Named::Left),
        None,
        Some(Named::Right),
        Some(Named::Home),
        Some(Named::Up),
        Some(Named::PageUp),
        Some(Named::Insert),
        Some(Named::Delete),
    ];
    let i = usize::from(usage - 0x59);
    if digits {
        Key::Char(DIGITS[i])
    } else {
        NAV[i].map_or(Key::Unknown(usage), Key::Named)
    }
}

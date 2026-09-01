//! X11 keysyms used by RFB key events (RFC 6143 §7.5.4).

pub const XK_BACKSPACE: u32 = 0xff08;
pub const XK_TAB: u32 = 0xff09;
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_DELETE: u32 = 0xffff;
pub const XK_HOME: u32 = 0xff50;
pub const XK_LEFT: u32 = 0xff51;
pub const XK_UP: u32 = 0xff52;
pub const XK_RIGHT: u32 = 0xff53;
pub const XK_DOWN: u32 = 0xff54;
pub const XK_PAGE_UP: u32 = 0xff55;
pub const XK_PAGE_DOWN: u32 = 0xff56;
pub const XK_END: u32 = 0xff57;
pub const XK_INSERT: u32 = 0xff63;
pub const XK_SHIFT_L: u32 = 0xffe1;
pub const XK_SHIFT_R: u32 = 0xffe2;
pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_CONTROL_R: u32 = 0xffe4;
#[allow(dead_code)]
pub const XK_META_L: u32 = 0xffe7;
pub const XK_ALT_L: u32 = 0xffe9;
pub const XK_ALT_R: u32 = 0xffea;
pub const XK_SUPER_L: u32 = 0xffeb;
pub const XK_SUPER_R: u32 = 0xffec;
pub const XK_CAPS_LOCK: u32 = 0xffe5;
pub const XK_F1: u32 = 0xffbe;
pub const XK_SPACE: u32 = 0x0020;

/// Ctrl, Alt, Delete — sent in that order for the toolbar action.
pub const CAD_KEYSYMS: [u32; 3] = [XK_CONTROL_L, XK_ALT_L, XK_DELETE];

/// Map a GPUI / OS key name (lowercase) plus shift to an X11 keysym.
///
/// US-centric for printable characters. Named keys (`enter`, `f8`, …) are
/// layout-independent.
pub fn keysym_of(key: &str, shift: bool) -> Option<u32> {
    let k = key.to_ascii_lowercase();
    if k.len() == 1 {
        let c = k.chars().next()?;
        if c.is_ascii_alphabetic() {
            let base = if shift { c.to_ascii_uppercase() } else { c };
            return Some(u32::from(base as u8));
        }
        if c.is_ascii_digit() {
            if shift {
                return Some(u32::from(shifted_digit(c)? as u8));
            }
            return Some(u32::from(c as u8));
        }
        if c.is_ascii_graphic() || c == ' ' {
            return Some(u32::from(c as u8));
        }
    }
    Some(match k.as_str() {
        "space" => XK_SPACE,
        "enter" | "return" => XK_RETURN,
        "tab" => XK_TAB,
        "escape" | "esc" => XK_ESCAPE,
        "backspace" => XK_BACKSPACE,
        "delete" | "del" => XK_DELETE,
        "left" | "arrowleft" => XK_LEFT,
        "right" | "arrowright" => XK_RIGHT,
        "up" | "arrowup" => XK_UP,
        "down" | "arrowdown" => XK_DOWN,
        "home" => XK_HOME,
        "end" => XK_END,
        "pageup" => XK_PAGE_UP,
        "pagedown" => XK_PAGE_DOWN,
        "insert" => XK_INSERT,
        "shift" | "leftshift" => XK_SHIFT_L,
        "rightshift" => XK_SHIFT_R,
        "control" | "ctrl" | "leftcontrol" => XK_CONTROL_L,
        "rightcontrol" | "rightctrl" => XK_CONTROL_R,
        "alt" | "option" | "leftalt" | "leftoption" => XK_ALT_L,
        "rightalt" | "rightoption" | "altgr" => XK_ALT_R,
        "meta" | "command" | "cmd" | "super" | "windows" | "win" | "leftmeta" => XK_SUPER_L,
        "rightmeta" | "rightcommand" | "rightsuper" => XK_SUPER_R,
        "f1" => XK_F1,
        "f2" => XK_F1 + 1,
        "f3" => XK_F1 + 2,
        "f4" => XK_F1 + 3,
        "f5" => XK_F1 + 4,
        "f6" => XK_F1 + 5,
        "f7" => XK_F1 + 6,
        "f8" => XK_F1 + 7,
        "f9" => XK_F1 + 8,
        "f10" => XK_F1 + 9,
        "f11" => XK_F1 + 10,
        "f12" => XK_F1 + 11,
        "-" if shift => u32::from(b'_'),
        "=" if shift => u32::from(b'+'),
        "[" if shift => u32::from(b'{'),
        "]" if shift => u32::from(b'}'),
        "\\" if shift => u32::from(b'|'),
        ";" if shift => u32::from(b':'),
        "'" if shift => u32::from(b'"'),
        "," if shift => u32::from(b'<'),
        "." if shift => u32::from(b'>'),
        "/" if shift => u32::from(b'?'),
        "`" if shift => u32::from(b'~'),
        "-" => u32::from(b'-'),
        "=" => u32::from(b'='),
        "[" => u32::from(b'['),
        "]" => u32::from(b']'),
        "\\" => u32::from(b'\\'),
        ";" => u32::from(b';'),
        "'" => u32::from(b'\''),
        "," => u32::from(b','),
        "." => u32::from(b'.'),
        "/" => u32::from(b'/'),
        "`" => u32::from(b'`'),
        _ => return None,
    })
}

fn shifted_digit(c: char) -> Option<char> {
    Some(match c {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        _ => return None,
    })
}

pub fn keysym_name(keysym: u32) -> &'static str {
    match keysym {
        XK_CONTROL_L => "Ctrl",
        XK_ALT_L => "Alt",
        XK_DELETE => "Del",
        XK_SUPER_L => "Win",
        XK_TAB => "Tab",
        XK_CAPS_LOCK => "Caps",
        XK_ESCAPE => "Esc",
        XK_RETURN => "Enter",
        _ => "Key",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters() {
        assert_eq!(keysym_of("a", false), Some(u32::from(b'a')));
        assert_eq!(keysym_of("a", true), Some(u32::from(b'A')));
    }

    #[test]
    fn named_keys() {
        assert_eq!(keysym_of("enter", false), Some(XK_RETURN));
        assert_eq!(keysym_of("f8", false), Some(XK_F1 + 7));
        assert_eq!(keysym_of("left", false), Some(XK_LEFT));
        assert_eq!(keysym_of("command", false), Some(XK_SUPER_L));
    }

    #[test]
    fn shifted_digits() {
        assert_eq!(keysym_of("1", true), Some(u32::from(b'!')));
    }
}

//! Console key decoding — pure logic, shared by the Windows console reader.
//!
//! Kept platform-neutral (no `windows-sys` types) so it can be unit-tested on
//! any host. The Windows input path had no test coverage at all, which is how
//! dropped auto-repeats, discarded surrogate pairs, and ignored modifier keys
//! all shipped.

#![cfg_attr(not(windows), allow(dead_code))]

// Virtual-key constants (defined locally to avoid pulling in
// Win32_UI_Input_KeyboardAndMouse just for these).
pub const VK_BACK: u16 = 0x08;
pub const VK_TAB: u16 = 0x09;
pub const VK_RETURN: u16 = 0x0D;
pub const VK_ESCAPE: u16 = 0x1B;
pub const VK_PRIOR: u16 = 0x21; // Page Up
pub const VK_NEXT: u16 = 0x22; // Page Down
pub const VK_END: u16 = 0x23;
pub const VK_HOME: u16 = 0x24;
pub const VK_LEFT: u16 = 0x25;
pub const VK_UP: u16 = 0x26;
pub const VK_RIGHT: u16 = 0x27;
pub const VK_DOWN: u16 = 0x28;
pub const VK_INSERT: u16 = 0x2D;
pub const VK_DELETE: u16 = 0x2E;
pub const VK_F1: u16 = 0x70;
pub const VK_F2: u16 = 0x71;
pub const VK_F3: u16 = 0x72;
pub const VK_F4: u16 = 0x73;
pub const VK_F5: u16 = 0x74;
pub const VK_F6: u16 = 0x75;
pub const VK_F7: u16 = 0x76;
pub const VK_F8: u16 = 0x77;
pub const VK_F9: u16 = 0x78;
pub const VK_F10: u16 = 0x79;
pub const VK_F11: u16 = 0x7A;
pub const VK_F12: u16 = 0x7B;

/// Modifier keys reported in `KEY_EVENT_RECORD.dwControlKeyState`.
///
/// The vkey table used to emit plain sequences unconditionally, so a
/// configured Shift-Tab or Ctrl-Arrow binding could never match on Windows —
/// the modified sequence `config::matches_modified` looks for was never
/// produced.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Modifiers {
    shift: bool,
    ctrl: bool,
    alt: bool,
}

impl Modifiers {
    pub fn from_control_key_state(state: u32) -> Self {
        const SHIFT_PRESSED: u32 = 0x0010;
        const RIGHT_ALT_PRESSED: u32 = 0x0001;
        const LEFT_ALT_PRESSED: u32 = 0x0002;
        const RIGHT_CTRL_PRESSED: u32 = 0x0004;
        const LEFT_CTRL_PRESSED: u32 = 0x0008;
        Self {
            shift: state & SHIFT_PRESSED != 0,
            ctrl: state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0,
            alt: state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0,
        }
    }

    pub fn any(self) -> bool {
        self.shift || self.ctrl || self.alt
    }

    /// xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    pub fn xterm_param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }
}

/// `CSI 1;<mod><final>` — the xterm encoding for a modified cursor key.
fn write_csi_modified(scratch: &mut [u8; 8], param: u8, final_byte: u8) -> usize {
    let digits = param.to_string();
    let mut n = 0;
    for b in b"\x1b[1;" {
        scratch[n] = *b;
        n += 1;
    }
    for b in digits.as_bytes() {
        scratch[n] = *b;
        n += 1;
    }
    scratch[n] = final_byte;
    n + 1
}

/// `CSI <code>~` or `CSI <code>;<mod>~` — the VT encoding for editing keys.
fn write_csi_tilde(scratch: &mut [u8; 8], code: u8, param: Option<u8>) -> usize {
    let mut n = 0;
    for b in b"\x1b[" {
        scratch[n] = *b;
        n += 1;
    }
    for b in code.to_string().as_bytes() {
        scratch[n] = *b;
        n += 1;
    }
    if let Some(param) = param {
        scratch[n] = b';';
        n += 1;
        for b in param.to_string().as_bytes() {
            scratch[n] = *b;
            n += 1;
        }
    }
    scratch[n] = b'~';
    n + 1
}

/// Bytes for a virtual key, honoring modifiers.
pub fn vkey_sequence(vk: u16, mods: Modifiers, scratch: &mut [u8; 8]) -> Option<&[u8]> {
    match vk {
        VK_BACK => Some(b"\x7f"),
        // Shift+Tab is backtab; Ctrl+Tab has no standard sequence.
        VK_TAB => Some(if mods.shift && !mods.ctrl {
            &b"\x1b[Z"[..]
        } else {
            &b"\t"[..]
        }),
        VK_RETURN => Some(b"\r"),
        VK_ESCAPE => Some(b"\x1b"),
        VK_UP | VK_DOWN | VK_RIGHT | VK_LEFT | VK_HOME | VK_END => {
            let final_byte = match vk {
                VK_UP => b'A',
                VK_DOWN => b'B',
                VK_RIGHT => b'C',
                VK_LEFT => b'D',
                VK_HOME => b'H',
                _ => b'F',
            };
            if mods.any() {
                let n = write_csi_modified(scratch, mods.xterm_param(), final_byte);
                Some(&scratch[..n])
            } else {
                scratch[0] = 0x1b;
                scratch[1] = b'[';
                scratch[2] = final_byte;
                Some(&scratch[..3])
            }
        }
        VK_INSERT | VK_DELETE | VK_PRIOR | VK_NEXT => {
            let code = match vk {
                VK_INSERT => 2u8,
                VK_DELETE => 3,
                VK_PRIOR => 5,
                _ => 6,
            };
            let param = mods.any().then(|| mods.xterm_param());
            let n = write_csi_tilde(scratch, code, param);
            Some(&scratch[..n])
        }
        VK_F1 => Some(b"\x1bOP"),
        VK_F2 => Some(b"\x1bOQ"),
        VK_F3 => Some(b"\x1bOR"),
        VK_F4 => Some(b"\x1bOS"),
        VK_F5 => Some(b"\x1b[15~"),
        VK_F6 => Some(b"\x1b[17~"),
        VK_F7 => Some(b"\x1b[18~"),
        VK_F8 => Some(b"\x1b[19~"),
        VK_F9 => Some(b"\x1b[20~"),
        VK_F10 => Some(b"\x1b[21~"),
        VK_F11 => Some(b"\x1b[23~"),
        VK_F12 => Some(b"\x1b[24~"),
        _ => None,
    }
}

/// Reassemble a UTF-16 surrogate pair. Windows delivers a non-BMP character
/// (an emoji, say) as two `KEY_EVENT`s, one per code unit. Passing each unit
/// to `char::from_u32` yielded `None` for both halves, so the character was
/// silently dropped and could never be typed or pasted.
pub fn decode_utf16_unit(pending: &std::cell::Cell<u16>, unit: u16) -> Option<char> {
    let held = pending.get();
    if (0xD800..0xDC00).contains(&unit) {
        pending.set(unit);
        return None; // wait for the low half
    }
    pending.set(0);
    if (0xDC00..0xE000).contains(&unit) {
        if held == 0 {
            return None; // stray low surrogate
        }
        let cp = 0x1_0000u32 + ((held as u32 - 0xD800) << 10) + (unit as u32 - 0xDC00);
        return char::from_u32(cp);
    }
    char::from_u32(unit as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const SHIFT: u32 = 0x0010;
    const LEFT_CTRL: u32 = 0x0008;
    const LEFT_ALT: u32 = 0x0002;

    fn seq(vk: u16, state: u32) -> Vec<u8> {
        let mods = Modifiers::from_control_key_state(state);
        let mut scratch = [0u8; 8];
        vkey_sequence(vk, mods, &mut scratch).unwrap().to_vec()
    }

    /// Unmodified keys keep their plain sequences.
    #[test]
    fn plain_keys_are_unchanged() {
        assert_eq!(seq(VK_UP, 0), b"\x1b[A");
        assert_eq!(seq(VK_DOWN, 0), b"\x1b[B");
        assert_eq!(seq(VK_TAB, 0), b"\t");
        assert_eq!(seq(VK_BACK, 0), b"\x7f");
        assert_eq!(seq(VK_DELETE, 0), b"\x1b[3~");
    }

    /// The vkey table used to emit plain sequences regardless of modifiers, so
    /// `config::matches_modified` could never match on Windows.
    #[test]
    fn modified_arrows_use_the_xterm_encoding() {
        // 1 + ctrl(4) = 5
        assert_eq!(seq(VK_UP, LEFT_CTRL), b"\x1b[1;5A");
        // 1 + shift(1) = 2
        assert_eq!(seq(VK_DOWN, SHIFT), b"\x1b[1;2B");
        // 1 + shift(1) + alt(2) + ctrl(4) = 8
        assert_eq!(seq(VK_LEFT, SHIFT | LEFT_ALT | LEFT_CTRL), b"\x1b[1;8D");
    }

    /// Shift+Tab is backtab; Ctrl+Tab has no standard sequence and stays Tab.
    #[test]
    fn shift_tab_is_backtab() {
        assert_eq!(seq(VK_TAB, SHIFT), b"\x1b[Z");
        assert_eq!(seq(VK_TAB, LEFT_CTRL), b"\t");
        assert_eq!(seq(VK_TAB, SHIFT | LEFT_CTRL), b"\t");
    }

    #[test]
    fn modified_editing_keys_carry_the_parameter() {
        assert_eq!(seq(VK_DELETE, LEFT_CTRL), b"\x1b[3;5~");
        assert_eq!(seq(VK_PRIOR, SHIFT), b"\x1b[5;2~");
    }

    #[test]
    fn xterm_parameter_matches_the_config_encoding() {
        let ctrl = Modifiers::from_control_key_state(LEFT_CTRL);
        assert_eq!(ctrl.xterm_param(), 5);
        let shift = Modifiers::from_control_key_state(SHIFT);
        assert_eq!(shift.xterm_param(), 2);
        assert!(!Modifiers::default().any());
    }

    /// A non-BMP character arrives as two records, one per UTF-16 unit.
    /// Decoding each unit alone yielded `None` for both, dropping the input.
    #[test]
    fn surrogate_pairs_are_reassembled() {
        let pending = Cell::new(0u16);
        // U+1F600 GRINNING FACE = D83D DE00
        assert_eq!(decode_utf16_unit(&pending, 0xD83D), None);
        assert_eq!(decode_utf16_unit(&pending, 0xDE00), Some('😀'));
        assert_eq!(pending.get(), 0);
    }

    #[test]
    fn bmp_characters_decode_directly() {
        let pending = Cell::new(0u16);
        assert_eq!(decode_utf16_unit(&pending, 'a' as u16), Some('a'));
        assert_eq!(decode_utf16_unit(&pending, 0x00E9), Some('é'));
        // Ctrl-A arrives as the C0 control byte.
        assert_eq!(decode_utf16_unit(&pending, 0x0001), Some('\u{1}'));
    }

    /// A stray low surrogate must not panic or emit garbage.
    #[test]
    fn stray_surrogates_are_dropped() {
        let pending = Cell::new(0u16);
        assert_eq!(decode_utf16_unit(&pending, 0xDE00), None);
        // A high surrogate followed by a normal character abandons the pair.
        assert_eq!(decode_utf16_unit(&pending, 0xD83D), None);
        assert_eq!(decode_utf16_unit(&pending, 'x' as u16), Some('x'));
        assert_eq!(pending.get(), 0);
    }

    /// The sequences this module emits must be exactly the ones
    /// `config::KeyBinding::matches` looks for — otherwise a configured
    /// Ctrl-Down or Shift-Tab binding still never fires on Windows.
    #[test]
    fn emitted_sequences_satisfy_the_configured_bindings() {
        use crate::config::KeyBinding;

        let ctrl_down = KeyBinding {
            key: "down".into(),
            shift: false,
            control: true,
        };
        assert!(ctrl_down.matches(&seq(VK_DOWN, LEFT_CTRL)));

        let shift_up = KeyBinding {
            key: "up".into(),
            shift: true,
            control: false,
        };
        assert!(shift_up.matches(&seq(VK_UP, SHIFT)));

        let shift_tab = KeyBinding {
            key: "tab".into(),
            shift: true,
            control: false,
        };
        assert!(shift_tab.matches(&seq(VK_TAB, SHIFT)));

        // And a plain binding is still satisfied by the plain sequence.
        let plain_down = KeyBinding::new("down");
        assert!(plain_down.matches(&seq(VK_DOWN, 0)));
        // ...but not by the modified one.
        assert!(!plain_down.matches(&seq(VK_DOWN, LEFT_CTRL)));
    }
}

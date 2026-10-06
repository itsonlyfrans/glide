//! USB keyboard page 0x07 to IBM Set-1 scan codes. Bit 8 denotes an E0 prefix.
use glide_platform::{BackendError, InputEvent, InputEventKind, Key};
use windows_sys::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};

pub(crate) const MAGIC: usize = 0x474c_4944;

const PAIRS: &[(u16, u16)] = &[
    (4, 0x1e),
    (5, 0x30),
    (6, 0x2e),
    (7, 0x20),
    (8, 0x12),
    (9, 0x21),
    (10, 0x22),
    (11, 0x23),
    (12, 0x17),
    (13, 0x24),
    (14, 0x25),
    (15, 0x26),
    (16, 0x32),
    (17, 0x31),
    (18, 0x18),
    (19, 0x19),
    (20, 0x10),
    (21, 0x13),
    (22, 0x1f),
    (23, 0x14),
    (24, 0x16),
    (25, 0x2f),
    (26, 0x11),
    (27, 0x2d),
    (28, 0x15),
    (29, 0x2c),
    (30, 0x02),
    (31, 0x03),
    (32, 0x04),
    (33, 0x05),
    (34, 0x06),
    (35, 0x07),
    (36, 0x08),
    (37, 0x09),
    (38, 0x0a),
    (39, 0x0b),
    (40, 0x1c),
    (41, 0x01),
    (42, 0x0e),
    (43, 0x0f),
    (44, 0x39),
    (45, 0x0c),
    (46, 0x0d),
    (47, 0x1a),
    (48, 0x1b),
    (49, 0x2b),
    (51, 0x27),
    (52, 0x28),
    (53, 0x29),
    (54, 0x33),
    (55, 0x34),
    (56, 0x35),
    (57, 0x3a),
    (58, 0x3b),
    (59, 0x3c),
    (60, 0x3d),
    (61, 0x3e),
    (62, 0x3f),
    (63, 0x40),
    (64, 0x41),
    (65, 0x42),
    (66, 0x43),
    (67, 0x44),
    (68, 0x57),
    (69, 0x58),
    (70, 0x137),
    (71, 0x46),
    (73, 0x152),
    (74, 0x147),
    (75, 0x149),
    (76, 0x153),
    (77, 0x14f),
    (78, 0x151),
    (79, 0x14d),
    (80, 0x14b),
    (81, 0x150),
    (82, 0x148),
    (83, 0x45),
    (84, 0x135),
    (85, 0x37),
    (86, 0x4a),
    (87, 0x4e),
    (88, 0x11c),
    (89, 0x4f),
    (90, 0x50),
    (91, 0x51),
    (92, 0x4b),
    (93, 0x4c),
    (94, 0x4d),
    (95, 0x47),
    (96, 0x48),
    (97, 0x49),
    (98, 0x52),
    (99, 0x53),
    (100, 0x56),
    (101, 0x15d),
    (103, 0x59),
    (104, 0x64),
    (105, 0x65),
    (106, 0x66),
    (107, 0x67),
    (108, 0x68),
    (109, 0x69),
    (110, 0x6a),
    (111, 0x6b),
    (112, 0x6c),
    (113, 0x6d),
    (114, 0x6e),
    (115, 0x76),
    (135, 0x73),
    (136, 0x70),
    (137, 0x7d),
    (138, 0x79),
    (139, 0x7b),
    (224, 0x1d),
    (225, 0x2a),
    (226, 0x38),
    (227, 0x15b),
    (228, 0x11d),
    (229, 0x36),
    (230, 0x138),
    (231, 0x15c),
];
const fn forward() -> [u16; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < PAIRS.len() {
        table[PAIRS[i].0 as usize] = PAIRS[i].1;
        i += 1;
    }
    table
}
const fn reverse() -> [u16; 512] {
    let mut table = [0; 512];
    let mut i = 0;
    while i < PAIRS.len() {
        table[PAIRS[i].1 as usize] = PAIRS[i].0;
        i += 1;
    }
    table
}
const FORWARD: [u16; 256] = forward();
const REVERSE: [u16; 512] = reverse();

pub(crate) fn scan(key: Key) -> Result<u16, BackendError> {
    let code = FORWARD.get(key.0 as usize).copied().unwrap_or(0);
    if code == 0 {
        Err(BackendError::Unsupported)
    } else {
        Ok(code)
    }
}

pub(crate) fn key_input(key: Key, down: bool) -> Result<INPUT, BackendError> {
    let up = if down { 0 } else { KEYEVENTF_KEYUP };
    // Pause has an E1 sequence, which SendInput's E0 flag cannot represent. VK_PAUSE lets
    // Windows synthesize it correctly; PrintScreen uses E0 37 without a fake Shift hold.
    let ki = if key.0 == 72 {
        KEYBDINPUT {
            wVk: VK_PAUSE,
            dwFlags: up,
            dwExtraInfo: MAGIC,
            ..Default::default()
        }
    } else {
        let code = scan(key)?;
        KEYBDINPUT {
            wScan: code & 0xff,
            dwFlags: KEYEVENTF_SCANCODE
                | up
                | if code & 0x100 != 0 {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    0
                },
            dwExtraInfo: MAGIC,
            ..Default::default()
        }
    };
    Ok(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    })
}

/// Keys some software sends without a usable scan code (Logitech Options+, remappers, on-screen keyboards): the
/// virtual-key code still says which key it is.
fn hid_from_vk(vk: u32) -> u16 {
    match vk {
        0x41..=0x5a => (vk - 0x41 + 4) as u16,
        0x31..=0x39 => (vk - 0x31 + 30) as u16,
        0x30 => 39,
        0x70..=0x7b => (vk - 0x70 + 58) as u16,
        _ => match vk as u16 {
            VK_RETURN => 40,
            VK_ESCAPE => 41,
            VK_BACK => 42,
            VK_TAB => 43,
            VK_SPACE => 44,
            VK_CAPITAL => 57,
            VK_INSERT => 73,
            VK_HOME => 74,
            VK_PRIOR => 75,
            VK_DELETE => 76,
            VK_END => 77,
            VK_NEXT => 78,
            VK_RIGHT => 79,
            VK_LEFT => 80,
            VK_DOWN => 81,
            VK_UP => 82,
            VK_LCONTROL | VK_CONTROL => 224,
            VK_LSHIFT | VK_SHIFT => 225,
            VK_LMENU | VK_MENU => 226,
            VK_LWIN => 227,
            VK_RCONTROL => 228,
            VK_RSHIFT => 229,
            VK_RMENU => 230,
            VK_RWIN => 231,
            _ => 0,
        },
    }
}

pub(crate) fn translate_key(data: KBDLLHOOKSTRUCT) -> Option<InputEvent> {
    let hid = if data.vkCode == VK_PAUSE as u32 {
        72
    } else if data.vkCode == VK_SNAPSHOT as u32 {
        70
    } else {
        let code = (data.scanCode & 0xff)
            | if data.flags & LLKHF_EXTENDED != 0 {
                0x100
            } else {
                0
            };
        match REVERSE[code as usize] {
            0 => hid_from_vk(data.vkCode),
            hid => hid,
        }
    };
    (hid != 0).then_some(InputEvent {
        kind: InputEventKind::Key {
            key: Key(hid),
            down: data.flags & LLKHF_UP == 0,
        },
        injected: data.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0
            || data.dwExtraInfo == MAGIC,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_supported_scan_round_trips_and_distinguishes_numpad() {
        for &(hid, code) in PAIRS {
            assert_eq!(scan(Key(hid)), Ok(code));
            assert_eq!(
                translate_key(KBDLLHOOKSTRUCT {
                    scanCode: (code & 255) as u32,
                    flags: if code & 256 != 0 { LLKHF_EXTENDED } else { 0 },
                    ..Default::default()
                })
                .map(|e| e.kind),
                Some(InputEventKind::Key {
                    key: Key(hid),
                    down: true
                })
            );
        }
        assert_ne!(scan(Key(88)), scan(Key(40)));
        assert_ne!(scan(Key(95)), scan(Key(74)));
        assert!(scan(Key(65535)).is_err());
    }
    #[test]
    fn injection_and_pause_printscreen_are_classified() {
        for flag in [LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED] {
            assert!(translate_key(KBDLLHOOKSTRUCT {
                scanCode: 0x1e,
                flags: flag | LLKHF_UP,
                ..Default::default()
            })
            .is_some_and(
                |e| e.injected && matches!(e.kind, InputEventKind::Key { down: false, .. })
            ));
        }
        for (vk, hid) in [(VK_PAUSE, 72), (VK_SNAPSHOT, 70)] {
            assert_eq!(
                translate_key(KBDLLHOOKSTRUCT {
                    vkCode: vk as u32,
                    ..Default::default()
                })
                .map(|e| e.kind),
                Some(InputEventKind::Key {
                    key: Key(hid),
                    down: true
                })
            );
            assert!(key_input(Key(hid), true).is_ok());
        }
    }

    #[test]
    fn independent_hid_fixtures_cover_scan_prefixes_and_numpad() {
        // Literal USB HID usage -> Set-1 fixtures. These intentionally do not use PAIRS,
        // FORWARD, or REVERSE so a bad table entry cannot validate itself.
        let fixtures: &[(u16, u16, u16)] = &[
            (4, 0x1e, 0x1e),    // A
            (30, 0x02, 0x02),   // 1
            (40, 0x1c, 0x1c),   // Main Enter
            (88, 0x1c, 0x11c),  // Keypad Enter (E0)
            (84, 0x35, 0x135),  // Keypad divide (E0)
            (70, 0x37, 0x137),  // Print Screen (E0)
            (74, 0x47, 0x147),  // Home (E0)
            (82, 0x48, 0x148),  // Up arrow (E0)
            (224, 0x1d, 0x1d),  // Left Ctrl
            (228, 0x1d, 0x11d), // Right Ctrl (E0)
            (225, 0x2a, 0x2a),  // Left Shift
            (229, 0x36, 0x36),  // Right Shift
            (226, 0x38, 0x38),  // Left Alt
            (230, 0x38, 0x138), // Right Alt (E0)
            (227, 0x5b, 0x15b), // Left GUI (E0)
            (231, 0x5c, 0x15c), // Right GUI (E0)
        ];

        for &(hid, set1, encoded) in fixtures {
            assert_eq!(scan(Key(hid)), Ok(encoded), "HID usage {hid}");
            let event = translate_key(KBDLLHOOKSTRUCT {
                scanCode: u32::from(set1),
                flags: if encoded & 0x100 != 0 {
                    LLKHF_EXTENDED
                } else {
                    0
                },
                ..Default::default()
            });
            assert_eq!(
                event.map(|event| event.kind),
                Some(InputEventKind::Key {
                    key: Key(hid),
                    down: true,
                }),
                "Set-1 code {encoded:#x}"
            );
        }
    }

    #[test]
    fn injection_fixtures_encode_pause_e0_keyup_and_magic_tag() {
        let main_enter = key_input(Key(40), true).expect("main Enter has a scan code");
        // SAFETY: `main_enter` is initialized as INPUT_KEYBOARD by key_input.
        let main_enter = unsafe { main_enter.Anonymous.ki };
        assert_eq!(main_enter.wVk, 0);
        assert_eq!(main_enter.wScan, 0x1c);
        assert_eq!(main_enter.dwFlags, KEYEVENTF_SCANCODE);
        assert_eq!(main_enter.dwExtraInfo, MAGIC);

        let keypad_enter = key_input(Key(88), false).expect("keypad Enter has a scan code");
        // SAFETY: `keypad_enter` is initialized as INPUT_KEYBOARD by key_input.
        let keypad_enter = unsafe { keypad_enter.Anonymous.ki };
        assert_eq!(keypad_enter.wScan, 0x1c);
        assert_eq!(
            keypad_enter.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        assert_eq!(keypad_enter.dwExtraInfo, MAGIC);

        let pause_up = key_input(Key(72), false).expect("Pause uses the native virtual key");
        // SAFETY: `pause_up` is initialized as INPUT_KEYBOARD by key_input.
        let pause_up = unsafe { pause_up.Anonymous.ki };
        assert_eq!(pause_up.wVk, VK_PAUSE);
        assert_eq!(pause_up.dwFlags, KEYEVENTF_KEYUP);
        assert_eq!(pause_up.dwExtraInfo, MAGIC);
    }

    #[test]
    fn hook_translation_marks_magic_and_rejects_unmapped_codes() {
        let tagged = translate_key(KBDLLHOOKSTRUCT {
            scanCode: 0x1e,
            dwExtraInfo: MAGIC,
            ..Default::default()
        });
        assert!(tagged.is_some_and(|event| event.injected));

        let unmapped = translate_key(KBDLLHOOKSTRUCT {
            scanCode: 0xff,
            ..Default::default()
        });
        assert!(unmapped.is_none());
    }

    // Bug: pressing Shift while controlling the Mac sent the cursor home, because a Shift without a usable scan code
    // (sent by keyboard software such as Logitech Options+, or flagged as extended) was unknown and capture gave up.
    #[test]
    fn keys_without_a_usable_scan_code_including_extended_shift_are_recognised() {
        for (vk, hid) in [
            (VK_LSHIFT, 225),
            (VK_RSHIFT, 229),
            (VK_SHIFT, 225),
            (VK_RETURN, 40),
            (0x41, 4),
        ] {
            assert_eq!(
                translate_key(KBDLLHOOKSTRUCT {
                    vkCode: vk as u32,
                    scanCode: 0,
                    ..Default::default()
                })
                .map(|e| e.kind),
                Some(InputEventKind::Key {
                    key: Key(hid),
                    down: true
                })
            );
        }
        // Bug: some keyboards (or their software) mark Shift as an extended key. Taking those for Windows' own
        // "fake" Shift presses dropped Shift entirely: no capitals and no Shift+Enter on the other computer.
        for (vk, scan, hid) in [(VK_LSHIFT, 0x2a, 225), (VK_RSHIFT, 0x36, 229)] {
            assert_eq!(
                translate_key(KBDLLHOOKSTRUCT {
                    vkCode: vk as u32,
                    scanCode: scan,
                    flags: LLKHF_EXTENDED,
                    ..Default::default()
                })
                .map(|e| e.kind),
                Some(InputEventKind::Key {
                    key: Key(hid),
                    down: true
                })
            );
        }
        // A real Shift still maps by scan code.
        assert_eq!(
            translate_key(KBDLLHOOKSTRUCT {
                vkCode: VK_RSHIFT as u32,
                scanCode: 0x36,
                ..Default::default()
            })
            .map(|e| e.kind),
            Some(InputEventKind::Key {
                key: Key(229),
                down: true
            })
        );
    }
}

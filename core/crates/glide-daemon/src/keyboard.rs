//! HID modifier translation policy and collision-safe injected-key tracking.

use glide_platform::{Key, Os};
use glide_proto::ipc::SwapCtrlCmd;

/// HID usage of the Tab key.
const TAB: u16 = 0x2B;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    pub key: Key,
    pub down: bool,
}

/// Maps physical HID events and remembers which source keys currently hold each target key.
/// This prevents a translated Ctrl and a physical GUI key from releasing the same target GUI
/// key while the other source key is still down.
#[derive(Clone, Debug)]
pub struct KeyTranslator {
    source_os: Os,
    target_os: Os,
    policy: SwapCtrlCmd,
    held: Vec<(Key, Key)>,
}

impl KeyTranslator {
    pub fn new(source_os: Os, target_os: Os, policy: SwapCtrlCmd) -> Self {
        Self {
            source_os,
            target_os,
            policy,
            held: Vec::new(),
        }
    }

    fn swap_active(&self) -> bool {
        match self.policy {
            SwapCtrlCmd::Never => false,
            SwapCtrlCmd::Always => true,
            SwapCtrlCmd::Auto => self.source_os != self.target_os,
        }
    }

    /// Return the HID usage the target should inject for this physical source usage.
    pub fn translated_usage(&self, key: Key) -> Key {
        if !self.swap_active() {
            return key;
        }
        // Shortcuts follow the target's habit: a Mac target wants Cmd where the Windows keyboard has Ctrl, a Windows
        // target wants Ctrl where the Mac keyboard has Cmd. The Win key, Ctrl on a Mac keyboard, and Alt/Option stay put.
        let target_is_mac = self.target_os == Os::Macos;
        match key.0 {
            0xE0 if target_is_mac => Key(0xE3),
            0xE4 if target_is_mac => Key(0xE7),
            0xE3 if !target_is_mac => Key(0xE0),
            0xE7 if !target_is_mac => Key(0xE4),
            _ => key,
        }
    }

    /// Like `event`, but also performs the app-switcher translation: pressing Tab while Alt (Windows keyboard) is held
    /// makes a Mac switch apps with Cmd+Tab, and Tab while Cmd (Mac keyboard) is held makes Windows switch with Alt+Tab.
    /// Cmd is Ctrl for ordinary shortcuts (Cmd+C is Ctrl+C), so the held modifier is re-pointed at the moment Tab goes
    /// down and stays re-pointed until it is released, which keeps the switcher open exactly as long as the user holds it.
    pub fn process(&mut self, key: Key, down: bool) -> Vec<KeyEvent> {
        let mut out = Vec::new();
        if down && key.0 == TAB && self.source_os != self.target_os && self.swap_active() {
            out.extend(self.hold_app_switcher_modifier());
        }
        out.extend(self.event(key, down));
        out
    }

    fn hold_app_switcher_modifier(&mut self) -> Vec<KeyEvent> {
        // (physical source modifier, modifier the target needs for its app switcher)
        let moves: [(u16, u16); 2] = if self.target_os == Os::Macos {
            [(0xE2, 0xE3), (0xE6, 0xE7)] // Alt -> Cmd
        } else {
            [(0xE3, 0xE2), (0xE7, 0xE6)] // Cmd -> Alt
        };
        let mut out = Vec::new();
        for (source, new_target) in moves {
            let Some(index) = self.held.iter().position(|(held, _)| held.0 == source) else {
                continue;
            };
            let old = self.held[index].1;
            if old.0 == new_target {
                continue;
            }
            self.held[index].1 = Key(new_target);
            if !self.held.iter().any(|(_, target)| *target == old) {
                out.push(KeyEvent {
                    key: old,
                    down: false,
                });
            }
            if self
                .held
                .iter()
                .filter(|(_, target)| target.0 == new_target)
                .count()
                == 1
            {
                out.push(KeyEvent {
                    key: Key(new_target),
                    down: true,
                });
            }
        }
        out
    }

    /// Process a physical event. Repeated downs pass through as repeats; duplicate source keys
    /// share a target key until the last matching source key is released.
    pub fn event(&mut self, key: Key, down: bool) -> Option<KeyEvent> {
        if down {
            if self.held.iter().any(|(source, _)| *source == key) {
                return Some(KeyEvent {
                    key: self.translated_usage(key),
                    down: true,
                });
            }
            let target = self.translated_usage(key);
            let target_was_held = self.held.iter().any(|(_, held)| *held == target);
            self.held.push((key, target));
            return (!target_was_held).then_some(KeyEvent {
                key: target,
                down: true,
            });
        }

        let index = self.held.iter().position(|(source, _)| *source == key)?;
        let (_, target) = self.held.remove(index);
        if self.held.iter().any(|(_, held)| *held == target) {
            None
        } else {
            Some(KeyEvent {
                key: target,
                down: false,
            })
        }
    }

    /// Replace held state from a modifier snapshot, emitting only target transitions that changed.
    pub fn sync_held(&mut self, keys: &[Key]) -> Vec<KeyEvent> {
        let old = std::mem::take(&mut self.held);
        let mut desired = Vec::with_capacity(keys.len());
        for key in keys {
            if desired.iter().any(|(source, _)| source == key) {
                continue;
            }
            desired.push((*key, self.translated_usage(*key)));
        }

        let mut events = Vec::new();
        for (_, target) in &old {
            if !desired
                .iter()
                .any(|(_, desired_target)| desired_target == target)
                && !events.iter().any(|event: &KeyEvent| event.key == *target)
            {
                events.push(KeyEvent {
                    key: *target,
                    down: false,
                });
            }
        }
        for (_, target) in &desired {
            if !old.iter().any(|(_, old_target)| old_target == target)
                && !events
                    .iter()
                    .any(|event: &KeyEvent| event.key == *target && event.down)
            {
                events.push(KeyEvent {
                    key: *target,
                    down: true,
                });
            }
        }
        self.held = desired;
        events
    }

    /// Release each target usage once and clear the source-held state.
    pub fn release_all(&mut self) -> Vec<KeyEvent> {
        let mut released = Vec::new();
        for (_, target) in self.held.drain(..) {
            if !released.contains(&target) {
                released.push(target);
            }
        }
        released
            .into_iter()
            .map(|key| KeyEvent { key, down: false })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFIERS: [u16; 8] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7];
    const OSES: [Os; 2] = [Os::Windows, Os::Macos];
    const POLICIES: [SwapCtrlCmd; 3] = [SwapCtrlCmd::Auto, SwapCtrlCmd::Always, SwapCtrlCmd::Never];

    #[test]
    fn modifier_translation_is_exhaustive_by_usage_os_and_policy() {
        for source in OSES {
            for target in OSES {
                for policy in POLICIES {
                    let translator = KeyTranslator::new(source, target, policy);
                    let should_swap = match policy {
                        SwapCtrlCmd::Auto => source != target,
                        SwapCtrlCmd::Always => true,
                        SwapCtrlCmd::Never => false,
                    };
                    for usage in MODIFIERS {
                        let to_mac = target == Os::Macos;
                        let expected = match (usage, should_swap, to_mac) {
                            (0xE0, true, true) => 0xE3,
                            (0xE4, true, true) => 0xE7,
                            (0xE3, true, false) => 0xE0,
                            (0xE7, true, false) => 0xE4,
                            _ => usage,
                        };
                        assert_eq!(translator.translated_usage(Key(usage)), Key(expected));
                    }
                    assert_eq!(translator.translated_usage(Key(0x04)), Key(0x04));
                }
            }
        }
    }

    #[test]
    fn every_modifier_source_target_policy_emits_matching_down_and_up() {
        for source in OSES {
            for target in OSES {
                for policy in POLICIES {
                    for usage in MODIFIERS {
                        let mut translator = KeyTranslator::new(source, target, policy);
                        let translated = translator.translated_usage(Key(usage));
                        assert_eq!(
                            translator.event(Key(usage), true),
                            Some(KeyEvent {
                                key: translated,
                                down: true,
                            }),
                            "source={source:?} target={target:?} policy={policy:?} usage={usage:#x}"
                        );
                        assert_eq!(
                            translator.event(Key(usage), false),
                            Some(KeyEvent {
                                key: translated,
                                down: false,
                            }),
                            "source={source:?} target={target:?} policy={policy:?} usage={usage:#x}"
                        );
                    }
                }
            }
        }
    }

    // Bug: Cmd+C on the MacBook reached the Windows PC as the Win key + C (opened Start) because only Ctrl->Cmd existed.
    #[test]
    fn mac_keyboard_cmd_becomes_ctrl_on_windows_so_shortcuts_work() {
        let mut translator = KeyTranslator::new(Os::Macos, Os::Windows, SwapCtrlCmd::Auto);
        let cmd = translator.event(Key(0xE3), true).expect("cmd down");
        assert_eq!(cmd.key, Key(0xE0), "left Cmd must arrive as left Ctrl");
        let right = translator.event(Key(0xE7), true).expect("right cmd down");
        assert_eq!(right.key, Key(0xE4), "right Cmd must arrive as right Ctrl");
        // Mac Ctrl, Option and Shift are left alone; the Win key never appears.
        assert_eq!(translator.translated_usage(Key(0xE2)), Key(0xE2));
        assert_eq!(translator.translated_usage(Key(0xE1)), Key(0xE1));
        for usage in MODIFIERS {
            assert_ne!(translator.translated_usage(Key(usage)), Key(0xE3));
            assert_ne!(translator.translated_usage(Key(usage)), Key(0xE7));
        }
    }

    // Bug: holding Cmd and Ctrl together on the Mac keyboard must not release Ctrl on Windows early.
    #[test]
    fn mac_cmd_and_ctrl_share_windows_ctrl_until_both_are_released() {
        let mut translator = KeyTranslator::new(Os::Macos, Os::Windows, SwapCtrlCmd::Auto);
        assert!(translator.event(Key(0xE3), true).is_some());
        assert_eq!(translator.event(Key(0xE0), true), None);
        assert_eq!(translator.event(Key(0xE3), false), None);
        assert_eq!(
            translator.event(Key(0xE0), false),
            Some(KeyEvent {
                key: Key(0xE0),
                down: false
            })
        );
    }

    fn down(key: u16) -> KeyEvent {
        KeyEvent {
            key: Key(key),
            down: true,
        }
    }
    fn up(key: u16) -> KeyEvent {
        KeyEvent {
            key: Key(key),
            down: false,
        }
    }

    // Bug: Alt+Tab on the Windows keyboard did nothing on the Mac (Alt stayed Option), so the Mac app switcher
    // could not be used from the Windows keyboard.
    #[test]
    fn windows_alt_tab_becomes_mac_cmd_tab_and_keeps_the_switcher_open_while_alt_is_held() {
        let mut t = KeyTranslator::new(Os::Windows, Os::Macos, SwapCtrlCmd::Auto);
        assert_eq!(t.process(Key(0xE2), true), vec![down(0xE2)]);
        assert_eq!(
            t.process(Key(TAB), true),
            vec![up(0xE2), down(0xE3), down(TAB)]
        );
        assert_eq!(t.process(Key(TAB), false), vec![up(TAB)]);
        assert_eq!(
            t.process(Key(TAB), true),
            vec![down(TAB)],
            "second Tab only cycles"
        );
        assert_eq!(t.process(Key(TAB), false), vec![up(TAB)]);
        assert_eq!(
            t.process(Key(0xE2), false),
            vec![up(0xE3)],
            "releasing Alt releases Cmd and picks the app"
        );
        assert!(t.release_all().is_empty());
    }

    // Bug: Cmd+Tab on the Mac keyboard switched browser tabs on Windows (Cmd became Ctrl) instead of apps.
    #[test]
    fn mac_cmd_tab_becomes_windows_alt_tab_but_cmd_c_stays_ctrl_c() {
        let mut t = KeyTranslator::new(Os::Macos, Os::Windows, SwapCtrlCmd::Auto);
        assert_eq!(t.process(Key(0xE3), true), vec![down(0xE0)]);
        assert_eq!(
            t.process(Key(TAB), true),
            vec![up(0xE0), down(0xE2), down(TAB)]
        );
        assert_eq!(t.process(Key(TAB), false), vec![up(TAB)]);
        assert_eq!(t.process(Key(0xE3), false), vec![up(0xE2)]);
        // Ordinary shortcuts are unchanged: Cmd+C reaches Windows as Ctrl+C.
        assert_eq!(t.process(Key(0xE3), true), vec![down(0xE0)]);
        assert_eq!(t.process(Key(0x06), true), vec![down(0x06)]);
        assert_eq!(t.process(Key(0x06), false), vec![up(0x06)]);
        assert_eq!(t.process(Key(0xE3), false), vec![up(0xE0)]);
    }

    #[test]
    fn shift_alt_tab_and_ctrl_alt_tab_keep_their_other_modifiers() {
        let mut t = KeyTranslator::new(Os::Windows, Os::Macos, SwapCtrlCmd::Auto);
        assert_eq!(t.process(Key(0xE1), true), vec![down(0xE1)]);
        assert_eq!(t.process(Key(0xE2), true), vec![down(0xE2)]);
        assert_eq!(
            t.process(Key(TAB), true),
            vec![up(0xE2), down(0xE3), down(TAB)],
            "Shift stays held for Shift+Cmd+Tab"
        );
        // With Ctrl already holding the Mac Cmd, Alt joins it without pressing Cmd a second time.
        let mut t = KeyTranslator::new(Os::Windows, Os::Macos, SwapCtrlCmd::Auto);
        assert_eq!(t.process(Key(0xE0), true), vec![down(0xE3)]);
        assert_eq!(t.process(Key(0xE2), true), vec![down(0xE2)]);
        assert_eq!(t.process(Key(TAB), true), vec![up(0xE2), down(TAB)]);
        assert_eq!(
            t.process(Key(0xE2), false),
            Vec::new(),
            "Ctrl still holds Cmd"
        );
        assert_eq!(t.process(Key(0xE0), false), vec![up(0xE3)]);
    }

    #[test]
    fn no_app_switcher_translation_between_the_same_system_or_with_swapping_off() {
        let mut same = KeyTranslator::new(Os::Windows, Os::Windows, SwapCtrlCmd::Auto);
        assert_eq!(same.process(Key(0xE2), true), vec![down(0xE2)]);
        assert_eq!(same.process(Key(TAB), true), vec![down(TAB)]);
        let mut never = KeyTranslator::new(Os::Windows, Os::Macos, SwapCtrlCmd::Never);
        assert_eq!(never.process(Key(0xE2), true), vec![down(0xE2)]);
        assert_eq!(never.process(Key(TAB), true), vec![down(TAB)]);
    }

    #[test]
    fn down_and_up_share_mapping_and_collision_releases_only_after_last_source() {
        let mut translator = KeyTranslator::new(Os::Windows, Os::Macos, SwapCtrlCmd::Auto);
        assert_eq!(
            translator.event(Key(0xE0), true),
            Some(KeyEvent {
                key: Key(0xE3),
                down: true
            })
        );
        assert_eq!(translator.event(Key(0xE3), true), None);
        assert_eq!(translator.event(Key(0xE0), false), None);
        assert_eq!(
            translator.event(Key(0xE3), false),
            Some(KeyEvent {
                key: Key(0xE3),
                down: false
            })
        );
        assert_eq!(translator.event(Key(0xE0), false), None);
    }

    #[test]
    fn snapshots_and_release_all_deduplicate_colliding_modifiers() {
        let mut translator = KeyTranslator::new(Os::Windows, Os::Macos, SwapCtrlCmd::Auto);
        assert_eq!(
            translator.sync_held(&[Key(0xE0), Key(0xE3), Key(0xE4), Key(0xE7)]),
            vec![
                KeyEvent {
                    key: Key(0xE3),
                    down: true
                },
                KeyEvent {
                    key: Key(0xE7),
                    down: true
                },
            ]
        );
        assert_eq!(
            translator.release_all(),
            vec![
                KeyEvent {
                    key: Key(0xE3),
                    down: false
                },
                KeyEvent {
                    key: Key(0xE7),
                    down: false
                },
            ]
        );
    }

    #[test]
    fn modifier_snapshots_are_deduplicated_and_keep_shared_target_held() {
        let mut translator = KeyTranslator::new(Os::Windows, Os::Macos, SwapCtrlCmd::Auto);
        assert_eq!(
            translator.sync_held(&[Key(0xE0), Key(0xE3), Key(0xE0)]),
            vec![KeyEvent {
                key: Key(0xE3),
                down: true,
            }]
        );
        assert!(translator.sync_held(&[Key(0xE3)]).is_empty());
        assert_eq!(
            translator.sync_held(&[]),
            vec![KeyEvent {
                key: Key(0xE3),
                down: false,
            }]
        );
    }
}

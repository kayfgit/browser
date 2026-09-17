//! Key presses belong to the surface that accepted their down event, through release.
use std::{collections::HashSet, hash::Hash};

#[derive(Debug, PartialEq)]
pub enum Route {
    Page,
    Shell,
    Suppress,
}

pub struct KeyOwnership<K> {
    shell: HashSet<K>,
}
impl<K> Default for KeyOwnership<K> {
    fn default() -> Self {
        Self {
            shell: HashSet::new(),
        }
    }
}
impl<K: Eq + Hash> KeyOwnership<K> {
    pub fn clear(&mut self) {
        self.shell.clear();
    }
    pub fn route(&mut self, key: K, pressed: bool, synthetic: bool, shell_mode: bool) -> Route {
        // Tao synthesizes releases on blur and presses on focus. These are state
        // synchronization, not fresh typing, and must not release shell ownership.
        if synthetic {
            return Route::Suppress;
        }
        if !pressed {
            return if self.shell.remove(&key) || shell_mode {
                Route::Suppress
            } else {
                Route::Page
            };
        }
        if self.shell.contains(&key) {
            return Route::Suppress;
        }
        if shell_mode {
            self.shell.insert(key);
            Route::Shell
        } else {
            Route::Page
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hint_press_stays_owned_across_blur_focus_and_repeat_until_real_release() {
        let mut keys = KeyOwnership::default();
        assert_eq!(keys.route('a', true, false, true), Route::Shell);
        assert_eq!(keys.route('a', false, true, false), Route::Suppress);
        assert_eq!(keys.route('a', true, true, false), Route::Suppress);
        assert_eq!(keys.route('a', true, false, false), Route::Suppress);
        assert_eq!(keys.route('b', true, false, false), Route::Page);
        assert_eq!(keys.route('a', false, false, false), Route::Suppress);
        assert_eq!(keys.route('a', true, false, false), Route::Page);
    }
    #[test]
    fn leaving_the_lab_does_not_leave_a_key_stuck_owned() {
        let mut keys = KeyOwnership::default();
        keys.route('i', true, false, true);
        keys.clear();
        assert_eq!(keys.route('i', true, false, false), Route::Page);
    }
}

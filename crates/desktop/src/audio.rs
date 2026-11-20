//! Tab sound: the speaker the strip shows on a tab whose pages are playing audio (or
//! are muted), and muting one tab (`:mutetab`). A tab here is a tab-strip entry, so
//! a split tab plays sound if any of its pages do, and muting it mutes all of them.
//!
//! Muting uses the engine's own per-page mute where it has one (WebView2), so the
//! page's player state is untouched; other engines fall back to the same script
//! toggle `:mute` uses, on just that page.

use crate::draw::{self, Rgb};
use crate::tabs::TabContent;
use crate::App;

/// What a tab-strip entry's speaker shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Sound {
    /// No page in it is making sound, and none is muted: no speaker.
    #[default]
    Silent,
    /// A page in it is making sound.
    Playing,
    /// A page in it is muted (and none of the others is making sound).
    Muted,
}

/// The speaker for a tab whose web pages report `(audible, muted)`. A muted page
/// still reports audible (the engine knows it would be), so it counts as muted.
pub(crate) fn sound_of(pages: impl IntoIterator<Item = (bool, bool)>) -> Sound {
    let mut sound = Sound::Silent;
    for (audible, muted) in pages {
        if audible && !muted {
            return Sound::Playing;
        }
        if muted {
            sound = Sound::Muted;
        }
    }
    sound
}

/// Whether toggling a tab's mute should mute it: yes while any of its pages can
/// still be heard, so one press always silences the whole tab.
pub(crate) fn should_mute(muted: impl IntoIterator<Item = bool>) -> bool {
    muted.into_iter().any(|m| !m)
}

impl App {
    /// The web tabs (indices into `tabs`) in tab-strip entry `pos`.
    fn web_tabs_in(&self, pos: usize) -> Vec<usize> {
        let Some(tree) = self.windows.get(pos) else {
            return Vec::new();
        };
        let mut leaves = Vec::new();
        tree.leaves(&mut leaves);
        leaves.retain(|&i| self.tabs.get(i).is_some_and(|t| t.page_state().is_some()));
        leaves
    }

    /// What tab-strip entry `pos`'s speaker shows.
    pub(crate) fn strip_sound(&self, pos: usize) -> Sound {
        sound_of(self.web_tabs_in(pos).into_iter().filter_map(|i| {
            let page = self.tabs[i].page_state()?;
            Some((page.audible(), page.muted()))
        }))
    }

    /// Mute or unmute the page in tab `i`. The engine's own mute where it has one,
    /// else the `:mute` script toggle on that page (which then stays muted if `:mute`
    /// is on for every tab).
    pub(crate) fn set_tab_muted(&self, i: usize, muted: bool) {
        let Some(TabContent::Web(view, page)) = self.tabs.get(i).map(|t| &t.content) else {
            return;
        };
        if view.set_muted(muted).is_err() {
            let on = muted || self.mute;
            let _ = view.evaluate_script(&format!(
                "window.__setToggle&&window.__setToggle('mute',{on})"
            ));
        }
        page.set_muted(muted);
    }

    /// `:mutetab [N]`: mute tab-strip entry N (1-based, as numbered in the strip), or
    /// the current tab, or unmute it if it's already muted.
    pub(crate) fn toggle_tab_mute(&mut self, pos: Option<usize>) {
        let Some(pos) = pos.or_else(|| self.active_window()) else {
            self.set_error("no tab to mute");
            return;
        };
        if pos >= self.windows.len() {
            self.set_error(format!("there's no tab {}", pos + 1));
            return;
        }
        let pages = self.web_tabs_in(pos);
        if pages.is_empty() {
            self.set_error(format!("tab {} has no web page to mute", pos + 1));
            return;
        }
        let mute = should_mute(
            pages
                .iter()
                .filter_map(|&i| self.tabs[i].page_state().map(|p| p.muted())),
        );
        for &i in &pages {
            self.set_tab_muted(i, mute);
        }
        self.set_status(format!(
            "tab {} {}",
            pos + 1,
            if mute { "muted" } else { "unmuted" }
        ));
        self.window.request_redraw();
    }
}

/// Paint a speaker `size` px square with its top-left at `(x, y)`: a box and cone in
/// `color`, then two sound waves while playing, or a red cross while muted. Drawn
/// from rectangles rather than a font glyph, so it looks the same in every font.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_speaker(
    buf: &mut [u32],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    size: usize,
    color: Rgb,
    sound: Sound,
) {
    if sound == Sound::Silent || size < 6 {
        return;
    }
    let s = size;
    let mid = y + s / 2;
    // The box: the left fifth of the icon, a third of its height.
    let box_w = (s / 5).max(1);
    draw::fill_rect(buf, w, h, x, mid - s / 6, x + box_w, mid + s / 6 + 1, color);
    // The cone: widens from the box's height to nearly the full height.
    let cone_w = (s * 3 / 10).max(2);
    for c in 0..cone_w {
        let half = s / 6 + (s / 2 - s / 6) * (c + 1) / cone_w;
        let cx = x + box_w + c;
        draw::fill_rect(
            buf,
            w,
            h,
            cx,
            mid.saturating_sub(half),
            cx + 1,
            mid + half,
            color,
        );
    }
    let right = x + box_w + cone_w;
    let t = (s / 10).max(1);
    match sound {
        Sound::Playing => {
            // Two waves, the outer one taller: short vertical strokes with their ends
            // stepped in, which reads as an arc at this size.
            for (gap, half) in [(s / 8 + 1, s / 5), (s / 3 + 1, s / 3)] {
                let wx = right + gap;
                draw::fill_rect(
                    buf,
                    w,
                    h,
                    wx,
                    mid - half + t,
                    wx + t,
                    mid + half - t + 1,
                    color,
                );
                draw::fill_rect(
                    buf,
                    w,
                    h,
                    wx.saturating_sub(t),
                    mid - half,
                    wx,
                    mid - half + t,
                    color,
                );
                draw::fill_rect(
                    buf,
                    w,
                    h,
                    wx.saturating_sub(t),
                    mid + half - t + 1,
                    wx,
                    mid + half + 1,
                    color,
                );
            }
        }
        Sound::Muted => {
            // A cross to the right of the cone.
            let cross = (x + s).saturating_sub(right + 1).max(3);
            let x0 = right + 1;
            let y0 = mid - cross / 2;
            for k in 0..cross {
                draw::fill_rect(buf, w, h, x0 + k, y0 + k, x0 + k + t, y0 + k + t, draw::ERR);
                let ry = y0 + cross - 1 - k;
                draw::fill_rect(buf, w, h, x0 + k, ry, x0 + k + t, ry + t, draw::ERR);
            }
        }
        Sound::Silent => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_plays_if_any_page_can_be_heard() {
        assert_eq!(sound_of([]), Sound::Silent);
        assert_eq!(sound_of([(false, false)]), Sound::Silent);
        assert_eq!(sound_of([(true, false)]), Sound::Playing);
        // A split tab: one muted page, one playing → you can hear it, so it's playing.
        assert_eq!(sound_of([(true, true), (true, false)]), Sound::Playing);
    }

    #[test]
    fn a_muted_page_shows_as_muted_even_while_it_plays() {
        assert_eq!(sound_of([(true, true)]), Sound::Muted);
        assert_eq!(sound_of([(false, true), (false, false)]), Sound::Muted);
    }

    #[test]
    fn one_toggle_silences_the_whole_tab_then_unmutes_it() {
        assert!(should_mute([false]));
        // Partly muted: mute the rest rather than unmuting the muted one.
        assert!(should_mute([true, false]));
        assert!(!should_mute([true, true]));
    }

    #[test]
    fn the_speaker_stays_inside_its_square() {
        let (w, h) = (40, 30);
        for sound in [Sound::Playing, Sound::Muted] {
            for size in [6, 9, 14, 20] {
                let mut buf = vec![0u32; w * h];
                draw_speaker(&mut buf, w, h, 10, 5, size, (255, 255, 255), sound);
                for (i, px) in buf.iter().enumerate() {
                    if *px != 0 {
                        let (px_x, px_y) = (i % w, i / w);
                        assert!(
                            (10..10 + size + 1).contains(&px_x)
                                && (5..5 + size + 1).contains(&px_y),
                            "{sound:?} at size {size} painted ({px_x},{px_y}) outside its square"
                        );
                    }
                }
                assert!(buf.iter().any(|p| *p != 0), "{sound:?} painted nothing");
            }
        }
        let mut buf = vec![0u32; w * h];
        draw_speaker(&mut buf, w, h, 10, 5, 14, (255, 255, 255), Sound::Silent);
        assert!(buf.iter().all(|p| *p == 0), "silent tabs get no speaker");
    }
}

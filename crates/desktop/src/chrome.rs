//! The native chrome: softbuffer painting of panes (read/pager/terminal/blank),
//! the tab bar, the command/status bar, and the welcome screen.

use std::num::NonZeroU32;

use anyhow::Result;

use crate::draw::{self, Painter};
use crate::find::FindState;
use crate::hints::NativeHint;
use crate::panes::{PaneRect, FOCUS_BORDER};
use crate::{pty_term, read_view, App, ModeKind, Tab, TERM_PAD};

/// Paint one pane's native content into `rect`: an engine-free read document,
/// the vim error/res pager, a terminal grid, or the blank-pane prompt. Web panes
/// paint nothing here (their webview HWND covers the rect). `focused` gates the
/// overlays that only apply to the active pane: find highlights, the read caret,
/// and hint badges. Everything is clipped to `rect` so it never bleeds into a
/// neighbouring pane. A free fn (not a method) so it can borrow individual App
/// fields while the render buffer separately borrows `self.surface`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_pane(
    t: &Tab,
    p: &Painter,
    find: &FindState,
    mode: ModeKind,
    native_hints: &[NativeHint],
    hint_input: &str,
    hint_new_tab: bool,
    focused: bool,
    rect: PaneRect,
    buf: &mut [u32],
    wz: usize,
    hz: usize,
) {
        const MARGIN: i32 = 8;
        let left = rect.x + MARGIN;
        let top = rect.y;
        let bottom = rect.y + rect.h;
        let right = rect.x + rect.w;
        let find_on = focused && (find.active || mode == ModeKind::Find);
        // Fill confined to the pane (clamped on all sides).
        let fill = |buf: &mut [u32], x0: i32, y0: i32, x1: i32, y1: i32, c: draw::Rgb| {
            let (x0, y0) = (x0.max(rect.x), y0.max(top));
            let (x1, y1) = (x1.min(right), y1.min(bottom));
            if x1 > x0 && y1 > y0 {
                draw::fill_rect(buf, wz, hz, x0 as usize, y0 as usize, x1 as usize, y1 as usize, c);
            }
        };

        if let Some(nr) = t.native() {
            let line_h = nr.layout.line_h;
            for (li, line) in nr.layout.lines.iter().enumerate() {
                let y_top = top - nr.scroll + li as i32 * line_h;
                if y_top + line_h < top || y_top > bottom {
                    continue;
                }
                if line.rule {
                    let ry = y_top + line_h / 2;
                    fill(buf, left, ry, right - MARGIN, ry + 1, draw::DIM);
                    continue;
                }
                let baseline = y_top + line_h * 3 / 4;
                if find_on {
                    let chars: Vec<char> = line.runs.iter().flat_map(|r| r.text.chars()).collect();
                    let base = left + line.indent;
                    for (mi, m) in find.matches.iter().enumerate() {
                        if m.line != li {
                            continue;
                        }
                        let s = m.start.min(chars.len());
                        let e = m.end.min(chars.len());
                        let x0 = line_col_x(&line.runs, s, base, p);
                        let x1 = line_col_x(&line.runs, e, base, p);
                        let col = if mi == find.current { draw::FIND_CUR } else { draw::FIND };
                        fill(buf, x0, y_top, x1, y_top + line_h, col);
                    }
                }
                if focused {
                    if let Some((s0, s1)) = nr.caret.as_ref().and_then(|c| c.selection_on_row(li)) {
                        let base = left + line.indent;
                        let x0 = line_col_x(&line.runs, s0, base, p);
                        let x1 = line_col_x(&line.runs, s1, base, p);
                        fill(buf, x0, y_top, x1, y_top + line_h, draw::SEL);
                    }
                }
                let mut x = left + line.indent;
                for run in &line.runs {
                    x = p.text_rect(
                        buf, wz, hz, x, baseline as usize, &run.text, run.color, left, right, top,
                        bottom,
                    );
                }
                if focused {
                    if let Some(caret) = &nr.caret {
                        if li == caret.cy {
                            let chars: Vec<char> =
                                line.runs.iter().flat_map(|r| r.text.chars()).collect();
                            let cx0 = line_col_x(&line.runs, caret.cx, left + line.indent, p);
                            let cwid = p.measure("M").max(1) as i32;
                            fill(buf, cx0, y_top, cx0 + cwid, y_top + line_h, draw::ACCENT);
                            if let Some(ch) = chars.get(caret.cx) {
                                p.text_rect(
                                    buf, wz, hz, cx0.max(left), baseline as usize, &ch.to_string(),
                                    draw::BG, left, right, top, bottom,
                                );
                            }
                        }
                    }
                }
            }
            if focused && mode == ModeKind::Hint {
                let lh = p.line_height();
                for hint in native_hints {
                    if !hint.label.starts_with(hint_input) {
                        continue;
                    }
                    let label = if hint_new_tab {
                        hint.label.to_uppercase()
                    } else {
                        hint.label.clone()
                    };
                    let lw = p.measure(&label);
                    let bx = hint.x.max(0);
                    let by = hint.y - (lh as i32) * 3 / 4;
                    fill(buf, bx, by, bx + lw as i32 + 4, by + lh as i32, (0xff, 0xd4, 0x00));
                    p.text_rect(
                        buf, wz, hz, bx + 2, hint.y.max(0) as usize, &label, (0x10, 0x10, 0x10),
                        left, right, top, bottom,
                    );
                }
            }
            return;
        }

        if let Some(vb) = t.vim() {
            let line_h = p.line_height() as i32;
            let cw = p.measure("M").max(1) as i32;
            let leftcol = vb.left;
            let col_x = |line: &[char], col: usize| -> i32 {
                if col <= leftcol {
                    return left;
                }
                let end = col.min(line.len());
                let slice: String = line[leftcol..end].iter().collect();
                let mut x = left + p.measure(&slice) as i32;
                if col > line.len() {
                    x += (col - line.len()) as i32 * cw;
                }
                x
            };
            for r in vb.top..vb.lines.len() {
                let y_top = top + (r - vb.top) as i32 * line_h;
                if y_top >= bottom {
                    break;
                }
                let line = &vb.lines[r];
                if let Some((s0, s1)) = vb.selection_on_row(r) {
                    fill(buf, col_x(line, s0), y_top, col_x(line, s1), y_top + line_h, draw::SEL);
                }
                if find_on {
                    for (mi, m) in find.matches.iter().enumerate() {
                        if m.line != r {
                            continue;
                        }
                        let col = if mi == find.current { draw::FIND_CUR } else { draw::FIND };
                        fill(buf, col_x(line, m.start), y_top, col_x(line, m.end), y_top + line_h, col);
                    }
                }
                let baseline = (y_top + line_h * 3 / 4) as usize;
                if vb.left < line.len() {
                    let text: String = line[vb.left..].iter().collect();
                    p.text_rect(buf, wz, hz, left, baseline, &text, draw::FG, left, right, top, bottom);
                }
                if focused && r == vb.cy {
                    let cx0 = col_x(line, vb.cx);
                    let cx1 = col_x(line, vb.cx + 1).max(cx0 + cw);
                    fill(buf, cx0, y_top, cx1, y_top + line_h, draw::FG);
                    if let Some(ch) = line.get(vb.cx) {
                        p.text_rect(
                            buf, wz, hz, cx0.max(left), baseline, &ch.to_string(), draw::BG, left,
                            right, top, bottom,
                        );
                    }
                }
            }
            return;
        }

        if let Some(s) = t.term() {
            let (cw, ch) = (p.measure("M").max(1) as i32, p.line_height() as i32);
            fill(buf, rect.x, top, right, bottom, pty_term::BG);
            pty_term::render(&s.pty, p, buf, wz, hz, rect.x + TERM_PAD, top, cw, ch, bottom);
            return;
        }

        if let Some(ai) = t.ai() {
            // The AI tab IS a vim buffer (rebuilt each frame by refresh_ai_layout),
            // so it renders like the pager below but with per-line colour and an
            // Insert-mode caret at the end of the input (last) line.
            let vb = &ai.buf;
            let line_h = p.line_height() as i32;
            let cw = p.measure("M").max(1) as i32;
            let leftcol = vb.left;
            let col_x = |line: &[char], col: usize| -> i32 {
                if col <= leftcol {
                    return left;
                }
                let end = col.min(line.len());
                let slice: String = line[leftcol..end].iter().collect();
                let mut x = left + p.measure(&slice) as i32;
                if col > line.len() {
                    x += (col - line.len()) as i32 * cw;
                }
                x
            };
            // Typing into the AI field (passthrough) shows a caret at the end of the
            // input line; in Normal it shows the vim block cursor instead.
            let typing = mode == ModeKind::Passthrough;
            for r in vb.top..vb.lines.len() {
                let y_top = top + (r - vb.top) as i32 * line_h;
                if y_top >= bottom {
                    break;
                }
                let line = &vb.lines[r];
                let color = ai.colors.get(r).copied().unwrap_or(draw::FG);
                if let Some((s0, s1)) = vb.selection_on_row(r) {
                    fill(buf, col_x(line, s0), y_top, col_x(line, s1), y_top + line_h, draw::SEL);
                }
                if find_on {
                    for (mi, m) in find.matches.iter().enumerate() {
                        if m.line != r {
                            continue;
                        }
                        let c = if mi == find.current { draw::FIND_CUR } else { draw::FIND };
                        fill(buf, col_x(line, m.start), y_top, col_x(line, m.end), y_top + line_h, c);
                    }
                }
                let baseline = (y_top + line_h * 3 / 4) as usize;
                if vb.left < line.len() {
                    let text: String = line[vb.left..].iter().collect();
                    p.text_rect(buf, wz, hz, left, baseline, &text, color, left, right, top, bottom);
                }
                // Typing: a caret at the end of the input line; Normal: the vim block
                // cursor on the current row (when this pane is focused).
                if focused && typing && r + 1 == vb.lines.len() {
                    let cx0 = col_x(line, line.len());
                    fill(buf, cx0, y_top, cx0 + cw, y_top + line_h, draw::ACCENT);
                } else if focused && !typing && r == vb.cy {
                    let cx0 = col_x(line, vb.cx);
                    let cx1 = col_x(line, vb.cx + 1).max(cx0 + cw);
                    fill(buf, cx0, y_top, cx1, y_top + line_h, draw::FG);
                    if let Some(ch) = line.get(vb.cx) {
                        p.text_rect(
                            buf, wz, hz, cx0.max(left), baseline, &ch.to_string(), draw::BG, left,
                            right, top, bottom,
                        );
                    }
                }
            }
            return;
        }

        // Blank pane: a quiet prompt centred in the rect.
        let msg = "empty pane — :open a page · :te terminal";
        let mw = p.measure(msg) as i32;
        let tx = rect.x + ((rect.w - mw) / 2).max(MARGIN);
        let ty = top + rect.h / 2;
        p.text_rect(buf, wz, hz, tx, ty as usize, msg, draw::DIM, left, right, top, bottom);
    }

/// Paint a "frozen" placeholder over a web pane whose webview is hidden+suspended
/// (`:freeze`). The content band is already cleared to the theme bg by the caller,
/// so this just centres a short note — the suspended pane reads as deliberately
/// paused instead of a blank/stale gap.
fn paint_frozen_pane(p: &Painter, buf: &mut [u32], wz: usize, hz: usize, rect: PaneRect) {
    const MARGIN: i32 = 8;
    let lh = p.line_height() as i32;
    let cy = rect.y + rect.h / 2;
    let centered = |buf: &mut [u32], y: i32, text: &str, color: draw::Rgb| {
        let tw = p.measure(text) as i32;
        let x = (rect.x + (rect.w - tw) / 2).max(rect.x + MARGIN);
        p.text_rect(buf, wz, hz, x, y as usize, text, color, rect.x, rect.x + rect.w, rect.y, rect.y + rect.h);
    };
    centered(buf, cy - lh, "frozen", draw::AI);
    centered(buf, cy + lh, ":unfreeze to resume this tab", draw::DIM);
}

/// Draw a 2px accent outline around the focused pane (only shown while split, as
/// the cue for which pane the keyboard acts on).
pub(crate) fn draw_pane_border(r: PaneRect, buf: &mut [u32], wz: usize, hz: usize, accent: draw::Rgb) {
    let (x0, y0, x1, y1) = (r.x.max(0), r.y.max(0), r.x + r.w, r.y + r.h);
    let t = FOCUS_BORDER;
    draw::fill_rect(buf, wz, hz, x0 as usize, y0 as usize, x1 as usize, (y0 + t) as usize, accent);
    draw::fill_rect(buf, wz, hz, x0 as usize, (y1 - t).max(y0) as usize, x1 as usize, y1 as usize, accent);
    draw::fill_rect(buf, wz, hz, x0 as usize, y0 as usize, (x0 + t) as usize, y1 as usize, accent);
    draw::fill_rect(buf, wz, hz, (x1 - t).max(x0) as usize, y0 as usize, x1 as usize, y1 as usize, accent);
}

impl App {
    pub(crate) fn draw(&mut self) -> Result<()> {
        // Keep the engine-free read layout current (cheap no-op unless something
        // that affects layout changed) before we read it for painting.
        self.refresh_read_layout();
        // Cache each AI pane's content height for scroll clamping (and bottom-stick).
        self.refresh_ai_layout();
        // Keep the active terminal's grid matched to the window/zoom.
        self.sync_active_term_size();
        let (w, h) = self.inner();
        // Gather all dynamic text + zoom-scaled metrics up front, while we can
        // still borrow &self.
        let tab_labels = self.tab_labels();
        // Copy the (Copy) chrome theme up front so the paint closures can read it
        // without holding a borrow of `self`.
        let theme = self.theme;
        let welcome = self.active.is_none();
        // The pane tiling: each (tab, rect) to paint + the divider rects. With no
        // split this is just the active tab filling the whole content band.
        let (panes, dividers) = self.pane_layout();
        // Hoisted before the `buf` borrow below (which holds `self.surface`): a method
        // call would borrow all of `self` and clash with that.
        let is_split = self.is_split();
        // We must repaint native content (and any divider/border) ourselves; only the
        // pure single-web-tab case can take the cheap bars-only damage present.
        let any_native = panes
            .iter()
            .any(|(t, _)| self.tabs.get(*t).is_some_and(|t| t.webview().is_none()));
        let bar_h = self.bar_h() as usize;
        let tab_h = self.tab_bar_h() as usize;
        // Minimized / degenerate size: skip the frame. The tab + command bars are a
        // FIXED height; in a window shorter than they are (e.g. during Show Desktop /
        // minimize, where the client area collapses) drawing them writes past the
        // bottom of the pixel buffer (buf.len() == w*h) and panics. Nothing is
        // visible when minimized anyway, so there's nothing to paint.
        if (h as usize) < tab_h + bar_h + 4 || w < 8 {
            return Ok(());
        }
        // Command bar: draw the `:`-line with horizontal scroll-to-caret so editing
        // a long URL (e.g. after `:edit`, caret parked at the end) keeps the caret —
        // and the tail of the URL — visible. `cmd` is Some((line, scroll_px)) in
        // Command mode and replaces the normal segment list; `caret` is the lit
        // block cursor (x, width) already shifted by the same scroll.
        const MARGIN: i32 = 8;
        let cw = ((self.zoom * 2.0).round() as i32).max(2);
        let (segments, cmd, caret, sel) = if matches!(self.mode, ModeKind::Command | ModeKind::Find)
        {
            // `:` for a command, `/` for a find-in-page search.
            let pre = if self.mode == ModeKind::Find { '/' } else { ':' };
            let line = format!("{pre}{}", self.command);
            let prefix = format!("{pre}{}", &self.command[..self.command_cursor]);
            let caret_un = MARGIN + self.painter.measure(&prefix) as i32;
            // Keep the caret a hair inside the right edge; scroll only when it would
            // otherwise fall off the end of the bar.
            let right_bound = (w as i32 - cw - 2).max(MARGIN);
            let scroll = (caret_un - right_bound).max(0);
            let caret = if self.cursor_on {
                Some(((caret_un - scroll) as usize, cw as usize))
            } else {
                None
            };
            // Selection highlight rect (x range), in the same scrolled coordinates,
            // clipped to the left margin.
            let sel = self.sel_range().map(|(a, b)| {
                let x_of = |k: usize| {
                    MARGIN - scroll
                        + self.painter.measure(&format!("{pre}{}", &self.command[..k])) as i32
                };
                (x_of(a).max(MARGIN).max(0) as usize, x_of(b).max(0) as usize)
            });
            // Remember the scroll so a mouse click in the bar maps to a caret offset.
            self.bar_cmd_scroll = scroll;
            (Vec::new(), Some((line, scroll)), caret, sel)
        } else {
            (self.bar_segments(), None, None, None)
        };
        // Quick-maths live result, shown right-aligned in the command bar while the
        // typed line is an arithmetic expression (`:20*8` → `= 160`).
        let math = (self.mode == ModeKind::Command)
            .then(|| self.math_preview())
            .flatten()
            .map(|r| format!("= {r}"));
        // Autocomplete ghost: the un-typed tail of the suggestion, drawn dim after
        // the command text (Tab / Ctrl+Right accepts it).
        let cmd_suffix = self
            .command_suggestion()
            .and_then(|s| s.strip_prefix(self.command.as_str()).map(str::to_string))
            .filter(|t| !t.is_empty());
        // Hovered-link target, shown right-aligned in the Normal-mode bar (like a
        // browser status bar). Only for a live web page; mutually exclusive with the
        // command-mode math readout above.
        let hover = (self.mode == ModeKind::Normal && self.active_webview().is_some())
            .then(|| self.hover_link.clone())
            .flatten();

        self.surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .map_err(|e| anyhow::anyhow!("resize: {e}"))?;
        let mut buf = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("buffer: {e}"))?;

        // `p` is a disjoint field borrow from `self.surface`, so it coexists with `buf`.
        let p = &self.painter;
        let (wz, hz) = (w as usize, h as usize);
        let bar_top = hz.saturating_sub(bar_h);

        let baseline = bar_top + (bar_h * 2 / 3);
        // Draw the opaque command bar; called LAST so nothing bleeds through it.
        let draw_bar = |buf: &mut [u32]| {
            // Chrome hidden (fullscreen, Normal mode) → bar height is 0. Drawing anyway
            // would paint the command line at `baseline == hz`, i.e. a sliver clipped at
            // the very bottom edge of the screen — the "command bar cut off in
            // fullscreen" bug. Skip it entirely; `:` un-hides the bar by switching mode.
            if bar_h == 0 {
                return;
            }
            draw::fill_band(buf, wz, hz, bar_top, hz, theme.bar_bg);
            if let Some((text, scroll)) = &cmd {
                // Selection highlight first, so the text paints on top of it.
                if let Some((sx0, sx1)) = sel {
                    let lh = p.line_height();
                    let y0 = baseline.saturating_sub(lh * 3 / 4);
                    let y1 = (baseline + lh / 6).min(hz);
                    draw::fill_rect(buf, wz, hz, sx0, y0, sx1, y1, draw::SEL);
                }
                // Command line, scrolled left by `scroll` px; clip at the left
                // margin so scrolled-off text doesn't bleed into the edge.
                let endx =
                    p.text_clipped(buf, wz, hz, MARGIN - *scroll, baseline, text, theme.bar_fg, MARGIN);
                // Autocomplete ghost text (dim) continuing from the caret.
                if let Some(sfx) = &cmd_suffix {
                    p.text_clipped(buf, wz, hz, endx, baseline, sfx, draw::DIM, MARGIN);
                }
            } else {
                let mut x = 8;
                for (text, color) in &segments {
                    x = p.text(buf, wz, hz, x, baseline, text, *color) + 6;
                }
            }
            if let Some((cx, cw)) = caret {
                let lh = p.line_height();
                let y0 = baseline.saturating_sub(lh * 3 / 4);
                let y1 = (baseline + lh / 6).min(hz);
                draw::fill_rect(buf, wz, hz, cx, y0, cx + cw, y1, theme.bar_fg);
            }
            // Right-aligned quick-maths result, painted last so it sits on top.
            if let Some(text) = &math {
                let tw = p.measure(text) as i32;
                let x = (wz as i32 - MARGIN - tw).max(0) as usize;
                p.text(buf, wz, hz, x, baseline, text, theme.accent);
            }
            // Right-aligned hovered-link URL, dimmed. Truncated to the right ~70% of
            // the bar so it doesn't swamp the page URL; a bg patch behind it keeps any
            // overlapping left text from bleeding through.
            if let Some(url) = &hover {
                let shown = elide_to_width(p, url, (wz as i32 * 7 / 10).max(40));
                let tw = p.measure(&shown) as i32;
                let x = (wz as i32 - MARGIN - tw).max(MARGIN);
                let lh = p.line_height();
                let y0 = baseline.saturating_sub(lh * 3 / 4);
                let y1 = (baseline + lh / 6).min(hz);
                draw::fill_rect(buf, wz, hz, (x - 6).max(0) as usize, y0, wz, y1, theme.bar_bg);
                p.text(buf, wz, hz, x as usize, baseline, &shown, draw::DIM);
            }
        };

        if welcome {
            // No engine running: paint the welcome screen, THEN the bar on top so a
            // long welcome list can't bleed into the command bar.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, theme.bg);
            draw_welcome(p, &mut buf, wz, hz, self.zoom as f32, theme.accent);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else if any_native || is_split || self.frozen {
            // At least one pane is native, we're split, or we're frozen: repaint the
            // content band ourselves. Native panes are drawn into their rects; LIVE web
            // panes are left to their webview HWNDs (which sit on top of our surface);
            // a FROZEN web pane is hidden, so paint a placeholder over its (now empty)
            // rect. Then the dividers, the focused-pane border (only while split), the
            // bars, and a full present.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, theme.bg);
            for (t, r) in &panes {
                let is_web = self.tabs.get(*t).is_some_and(|tb| tb.webview().is_some());
                if !is_web {
                    paint_pane(
                        &self.tabs[*t], p, &self.find, self.mode, &self.native_hints,
                        &self.hint_input, self.hint_new_tab, Some(*t) == self.active, *r,
                        &mut buf, wz, hz,
                    );
                } else if self.frozen {
                    paint_frozen_pane(p, &mut buf, wz, hz, *r);
                }
            }
            for d in &dividers {
                draw::fill_rect(
                    &mut buf, wz, hz, d.x.max(0) as usize, d.y.max(0) as usize,
                    (d.x + d.w) as usize, (d.y + d.h) as usize, draw::DIM,
                );
            }
            if is_split || self.mode == ModeKind::PaneMove {
                if let Some((_, r)) = panes.iter().find(|(t, _)| Some(*t) == self.active) {
                    // The pane being moved is highlighted yellow; the ordinary focused
                    // pane keeps the theme accent.
                    let col = if self.mode == ModeKind::PaneMove { draw::GRAB } else { theme.accent };
                    draw_pane_border(*r, &mut buf, wz, hz, col);
                }
            }
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, theme.bar_bg);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else {
            // Single web tab: a webview covers the whole content band, so we only
            // repaint the bars and present just those rects — never over the page.
            draw_bar(&mut buf);
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, theme.bar_bg);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            let mut damage = Vec::new();
            if tab_h > 0 {
                damage.push(softbuffer::Rect {
                    x: 0,
                    y: 0,
                    width: NonZeroU32::new(w).unwrap(),
                    height: NonZeroU32::new(tab_h as u32).unwrap(),
                });
            }
            if bar_h > 0 {
                damage.push(softbuffer::Rect {
                    x: 0,
                    y: bar_top as u32,
                    width: NonZeroU32::new(w).unwrap(),
                    height: NonZeroU32::new(bar_h as u32).unwrap(),
                });
            }
            buf.present_with_damage(&damage).map_err(|e| anyhow::anyhow!("present: {e}"))?;
        }
        Ok(())
    }

    /// (label, is_active, color) for each tab-strip entry (one per tmux-style window),
    /// in order. A split window is labelled by its focused pane with a ` ⁝N` pane-count
    /// suffix so it reads as one entry containing N panes. The `:ai` singleton has no
    /// window, so it's omitted — see [`window_strip`](App::window_strip).
    pub(crate) fn tab_labels(&self) -> Vec<(String, bool, draw::Rgb)> {
        let aw = self.active_window();
        self.window_strip()
            .into_iter()
            .enumerate()
            .filter_map(|(wi, (rep, panes))| {
                let t = self.tabs.get(rep)?;
                let active = Some(wi) == aw;
                let color = if t.term().is_some() {
                    draw::TERM
                } else if t.vim().is_some() {
                    draw::ERR
                } else if t.read {
                    draw::READ
                } else if t.research {
                    draw::RESEARCH
                } else if active {
                    self.theme.accent
                } else {
                    draw::DIM
                };
                // Terminals label themselves by the running program's OSC title
                // (vim → the open file, Claude Code → "Claude Code"); fall back to
                // the shell name until something sets a title. A blank split pane
                // reads as "new".
                let mut label = if t.is_blank() {
                    "new".to_string()
                } else {
                    t.term()
                        .and_then(|s| s.pty.title())
                        .map(|title| term_label(&title))
                        .unwrap_or_else(|| short_label(&t.url))
                };
                // A split window shows how many panes it holds, tmux-style.
                if panes > 1 {
                    label.push_str(&format!(" ⁝{panes}"));
                }
                Some((label, active, color))
            })
            .collect()
    }

    /// Whether the active tab is a read-mode tab.
    pub(crate) fn active_is_read(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.read).unwrap_or(false)
    }

    /// Whether the active tab is a research-mode tab.
    pub(crate) fn active_is_research(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.research).unwrap_or(false)
    }

    /// The command verb that re-opens the active tab in its own mode, for `:edit`.
    pub(crate) fn active_reopen_verb(&self) -> &'static str {
        match self.active.and_then(|i| self.tabs.get(i)) {
            Some(t) if t.research => "research",
            Some(t) if t.read => "read",
            Some(t) if t.nojs => "nojs",
            _ => "open",
        }
    }

    /// Build the bar as a sequence of (text, color) segments drawn left to right.
    pub(crate) fn bar_segments(&self) -> Vec<(String, draw::Rgb)> {
        // Themed chrome colours (the mode tags use the accent; typed text uses bar_fg).
        let accent = self.theme.accent;
        let fg = self.theme.bar_fg;
        match self.mode {
            // The blinking caret is drawn separately (at the byte cursor), so the
            // text segment is just the literal command line. (Command/Find are drawn
            // via the dedicated caret path in `draw`, so these arms are unreached.)
            ModeKind::Command => vec![(format!(":{}", self.command), fg)],
            ModeKind::Find => vec![(format!("/{}", self.command), fg)],
            ModeKind::Resize => vec![
                ("[RESIZE]".into(), accent),
                ("  hjkl resize window · Esc done".into(), draw::DIM),
            ],
            ModeKind::Move => vec![
                ("[MOVE]".into(), accent),
                ("  hjkl move window · Esc done".into(), draw::DIM),
            ],
            ModeKind::PaneResize => vec![
                ("[RESIZE PANE]".into(), accent),
                ("  hjkl resize · Esc done".into(), draw::DIM),
            ],
            ModeKind::PaneMove => vec![
                ("[MOVE PANE]".into(), draw::GRAB),
                ("  hjkl swap · Enter set · Esc cancel".into(), draw::DIM),
            ],
            ModeKind::Hint => vec![
                (if self.hint_new_tab { "[HINT ↗]" } else { "[HINT]" }.into(), accent),
                (format!(" {}", self.hint_input), fg),
                (
                    if self.hint_new_tab {
                        "   label opens a new tab · Esc cancel".into()
                    } else {
                        "   type a label (UPPERCASE = new tab) · Esc cancel".into()
                    },
                    draw::DIM,
                ),
            ],
            ModeKind::Caret => vec![
                ("[CARET]".into(), accent),
                ("  hjkl/w/b/0/$/gg/G move · v select · y yank · Esc exit".into(), draw::DIM),
            ],
            // Light field-typing mode (web only): reads as [INSERT], with the field's
            // page URL and the leave/promote hint.
            ModeKind::Insert => vec![
                ("[INSERT]".into(), accent),
                (self.active_url().unwrap_or("").to_string(), fg),
                ("   type into the field · Esc or click away to leave".into(), draw::DIM),
            ],
            // Sticky typing mode — always reads as [PASS], whatever the content; only the
            // trailing hint differs (how to leave / use this content).
            ModeKind::Passthrough => {
                if self.active_is_ai() {
                    let hint = if self.groq_key.is_none() {
                        "   paste your Groq key · Enter saves · Esc to leave"
                    } else {
                        "   ask anything · Enter sends · Ctrl+U clear · Esc to leave"
                    };
                    return vec![("[PASS]".into(), accent), (hint.into(), draw::DIM)];
                }
                if self.active_is_term() {
                    return vec![
                        ("[PASS]".into(), accent),
                        ("   typing to the shell · Ctrl+V paste · Ctrl+S to leave".into(), draw::DIM),
                    ];
                }
                let url = self.active_url().unwrap_or("").to_string();
                vec![
                    ("[PASS]".into(), accent),
                    (url, fg),
                    ("   every key to the page · Ctrl+S or Shift+Esc to leave".into(), draw::DIM),
                ]
            }
            ModeKind::Normal => {
                let label = match self.active_url() {
                    Some(url) => {
                        let n = self.tabs.len();
                        let i = self.active.map(|i| i + 1).unwrap_or(0);
                        // Idle: show the host only. Hovering reveals the full LIVE url
                        // — SPA navigations (e.g. a YouTube watch page) aren't reflected
                        // in the stored tab url, so read it from the webview.
                        let shown = if self.bar_hover {
                            self.current_url().unwrap_or_else(|| url.to_string())
                        } else {
                            bar_short_url(url)
                        };
                        format!("{i}/{n}  {shown}")
                    }
                    None => ":open <url>  (or press o)".to_string(),
                };
                let mut segs = vec![("[N]".into(), accent), (label, fg)];
                if self.frozen {
                    segs.push(("   [FROZEN]  :unfreeze".into(), draw::AI));
                }
                if self.active_is_read() {
                    segs.push(("   [read]".into(), draw::READ));
                    // Read-mode caret: show [VISUAL]/[VISUAL LINE] (or [CARET]) + hint.
                    if let Some(caret) = self
                        .active
                        .and_then(|i| self.tabs.get(i))
                        .and_then(|t| t.native())
                        .and_then(|n| n.caret.as_ref())
                    {
                        let m = caret.mode_label().unwrap_or("CARET");
                        segs.push((format!("   [{m}]"), accent));
                        segs.push(("  motions select · y yank · Esc exit".into(), draw::DIM));
                    }
                }
                if self.active_is_research() {
                    segs.push(("   [research]".into(), draw::RESEARCH));
                }
                if self.active_is_ai() {
                    let label = self
                        .active
                        .and_then(|i| self.tabs.get(i))
                        .and_then(|t| t.ai())
                        .and_then(|a| a.buf.mode_label());
                    match label {
                        Some(m) => {
                            segs.push((format!("   [{m}]"), draw::AI));
                            segs.push(("  motions select · y yank · Esc".into(), draw::DIM));
                        }
                        None => segs.push(("   [ai]  i: ask · H/L: chats · v/y select".into(), draw::AI)),
                    }
                }
                // Terminal: [term] live (i types), [COPY] in vi/copy mode.
                if self.active_is_term() {
                    if self.active_term_vi() {
                        segs.push((
                            "   [COPY]  hjkl/w/b move · f find · v select · y yank · i resume".into(),
                            draw::TERM,
                        ));
                    } else {
                        segs.push(("   [term]  i: type · Ctrl+S: copy-mode".into(), draw::TERM));
                    }
                }
                // Vim pager tabs (`:error`/`:errors`, `:res`): show [VISUAL]/[VISUAL
                // LINE] while selecting, else a hint keyed to the tab — the red [error]
                // hint must NOT bleed onto the `:res` monitor.
                if let Some(t) = self.active.and_then(|i| self.tabs.get(i)) {
                    if let Some(vb) = t.vim() {
                        match vb.mode_label() {
                            Some(m) => segs.push((format!("   [{m}]"), accent)),
                            None if t.url == "browser://error" => segs.push((
                                "   [error]  v select · y yank · yi( inner ()".into(),
                                draw::ERR,
                            )),
                            None => segs.push(("   v select · y yank".into(), draw::DIM)),
                        }
                    }
                }
                if self.nojs {
                    segs.push(("   [no-js]".into(), accent));
                }
                // Active find-in-page: show the query and (for native tabs) the
                // current/total match counter; `n`/`N` step, Esc clears.
                if self.find.active {
                    let label = if self.active_webview().is_some() {
                        format!("   /{}  · n/N · Esc", self.find.query)
                    } else {
                        format!("   {}  · n/N · Esc", self.find_count_label())
                    };
                    segs.push((label, draw::FIND_CUR));
                }
                if !self.status.is_empty() {
                    let color = if self.status_is_error {
                        draw::ERR
                    } else {
                        self.status_color.unwrap_or(draw::DIM)
                    };
                    segs.push((format!("   {}", self.status), color));
                }
                segs
            }
        }
    }
}

/// Pixel x of character column `col` within a read-view line, computed by
/// replicating exactly how the line is painted: the pen runs in f32 within a run
/// but is floored to an `i32` at every run boundary (because `text_clipped` takes
/// and returns an `i32` x). Using `measure()` of the flattened prefix instead drifts
/// to the right as the column and zoom grow (TODO #8) — the per-run floors add up.
pub(crate) fn line_col_x(runs: &[read_view::Run], col: usize, base: i32, p: &Painter) -> i32 {
    let mut x = base as f32;
    let mut c = 0usize;
    for run in runs {
        for ch in run.text.chars() {
            if c == col {
                return x as i32;
            }
            x += p.advance(ch);
            c += 1;
        }
        x = x.floor(); // text_clipped returns `pen as i32` between runs
    }
    x as i32
}

/// Compact display form of a URL for autocomplete: scheme + `www.` + trailing
/// slash stripped (e.g. `https://www.youtube.com/` → `youtube.com`). The result is
/// still openable (`resolve_target` re-adds the scheme).
pub(crate) fn history_display(url: &str) -> String {
    let s = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let s = s.strip_prefix("www.").unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// Compact the URL for the idle Normal-mode command bar: just the host (scheme and
/// `www.` stripped), with a trailing `/…` when a path or query was dropped — a cue
/// that there's more. The full URL shows again on hover or once the command bar is
/// open. Non-web URLs (`browser://…`, native tabs) are left untouched.
pub(crate) fn bar_short_url(url: &str) -> String {
    let Some(s) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else {
        return url.to_string();
    };
    let s = s.strip_prefix("www.").unwrap_or(s);
    match s.split_once('/') {
        Some((host, rest)) if !rest.trim_end_matches('/').is_empty() => format!("{host}/…"),
        Some((host, _)) => host.to_string(),
        None => s.to_string(),
    }
}

/// Truncate `s` to fit within `max_px` at the painter's current size, appending an
/// ellipsis if it was cut. Keeps the START of the string (host/path of a link — the
/// useful part), unlike a plain right-clip.
fn elide_to_width(p: &Painter, s: &str, max_px: i32) -> String {
    if p.measure(s) as i32 <= max_px {
        return s.to_string();
    }
    let ell_w = p.measure("…") as i32;
    let mut out = String::new();
    let mut w = 0i32;
    for ch in s.chars() {
        let cw = p.advance(ch) as i32;
        if w + cw + ell_w > max_px {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// A short tab label: the host without scheme/`www.`, truncated.
pub(crate) fn short_label(url: &str) -> String {
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = s.split('/').next().unwrap_or(s);
    let host = host.strip_prefix("www.").unwrap_or(host);
    truncate_label(host)
}

/// Turn a terminal's raw OSC title into a tidy tab label. Strips the editor suffix
/// vim/nvim append (`<file> (<dir>) - VIM`) and reduces a bare path to its final
/// component, so a shell sitting in `C:\projects\browser` reads as `browser` and an
/// open file reads as just its name; anything else (e.g. `Claude Code`) is kept.
pub(crate) fn term_label(title: &str) -> String {
    let t = title.trim();
    // vim/nvim: "<file> (<dir>) - VIM"/"- NVIM" → keep the part before " - ".
    let t = t.split(" - ").next().unwrap_or(t).trim();
    // Drop the trailing "(<dir>)" annotation vim appends after the filename.
    let t = t.split(" (").next().unwrap_or(t).trim();
    // A bare path → its last segment; otherwise leave the text alone.
    let last = t.rsplit(['\\', '/']).next().filter(|s| !s.is_empty()).unwrap_or(t);
    truncate_label(if last.is_empty() { t } else { last })
}

/// Cap a tab label at 22 characters, appending an ellipsis when it overflows.
pub(crate) fn truncate_label(s: &str) -> String {
    let mut label = s.to_string();
    if label.chars().count() > 22 {
        label = label.chars().take(21).collect::<String>();
        label.push('…');
    }
    label
}

/// Draw the top tab bar: `[1:host]` for the active tab, ` 2:host ` for others.
/// Read-mode tabs are tinted green.
pub(crate) fn draw_tab_bar(p: &Painter, buf: &mut [u32], w: usize, h: usize, labels: &[(String, bool, draw::Rgb)]) {
    // Hidden (fullscreen) or no visible tabs → zero height; drawing would paint the
    // labels as a clipped sliver at the very top edge, so skip it.
    if h == 0 {
        return;
    }
    let baseline = h * 2 / 3;
    let mut x = 8;
    for (i, (label, active, color)) in labels.iter().enumerate() {
        let color = *color;
        let text = if *active {
            format!("[{}:{}]", i + 1, label)
        } else {
            format!(" {}:{} ", i + 1, label)
        };
        x = p.text(buf, w, h, x, baseline, &text, color) + 6;
        if x > w.saturating_sub(40) {
            p.text(buf, w, h, x, baseline, "…", draw::DIM);
            break;
        }
    }
}

/// Paint the engine-free welcome screen: title + a key/command cheat-sheet.
/// `scale` is the global zoom factor so column offsets track the scaled font.
pub(crate) fn draw_welcome(p: &Painter, buf: &mut [u32], w: usize, h: usize, _scale: f32, accent: draw::Rgb) {
    let lh = p.line_height();
    // A clean splash: the name + tagline centered, with one quiet hint below.
    let name = "browser";
    let tag = "  — lightweight modal shell";
    let title_w = p.measure(name) + p.measure(tag);
    let tx = w.saturating_sub(title_w) / 2;
    let ty = h / 2 - lh;
    let after = p.text(buf, w, h, tx, ty, name, accent);
    p.text(buf, w, h, after, ty, tag, draw::DIM);
    let hint = ":open <url> to start   ·   :commands for all keybindings";
    let hint_w = p.measure(hint);
    p.text(buf, w, h, w.saturating_sub(hint_w) / 2, ty + lh * 2, hint, draw::DIM);
}

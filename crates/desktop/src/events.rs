//! The event loop: what the browser does with each window event and each of its own
//! [`UserEvent`]s, and when it next needs to wake up. `main` builds the window and the
//! [`App`], then hands every event to [`handle`].

use std::time::{Duration, Instant};

use tao::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use tao::event_loop::ControlFlow;

#[cfg(all(windows, feature = "servo-engine"))]
use crate::engines;
use crate::{clipboard_set, shellkeys, App, HintAct, ModeKind, UserEvent};

/// Handle one event from the tao event loop.
pub(crate) fn handle(app: &mut App, event: Event<'_, UserEvent>, control_flow: &mut ControlFlow) {
    #[cfg(all(windows, feature = "servo-engine"))]
    let event = engines::servo::intercept(app, event).unwrap_or(Event::UserEvent(
        UserEvent::Servo(engines::servo::Event::Wake),
    ));

    let event = match event {
        Event::UserEvent(UserEvent::Engine { view, event }) => Event::UserEvent(
            app.route_engine_event(view, *event)
                .unwrap_or(UserEvent::Redraw),
        ),
        event => event,
    };
    *control_flow = ControlFlow::Wait;
    match event {
        Event::NewEvents(StartCause::ResumeTimeReached { .. }) => on_timer(app),
        Event::WindowEvent { event, .. } => on_window_event(app, event, control_flow),
        Event::UserEvent(event) => on_user_event(app, event, control_flow),
        Event::LoopDestroyed => app.teardown(),
        Event::RedrawRequested(_) => {
            if let Err(e) = app.draw() {
                eprintln!("draw error: {e}");
            }
        }
        _ => {}
    }
    // Publish the key mode, so keys pressed in a focused page are routed for the
    // shell's current state (see `shellkeys`).
    #[cfg(all(windows, feature = "servo-engine"))]
    let servo_smoke = engines::servo::smoke::tick(app);
    shellkeys::set_mode(app.key_mode());
    if app.quit {
        app.teardown();
        *control_flow = ControlFlow::Exit;
    }

    #[cfg(not(all(windows, feature = "servo-engine")))]
    let servo_smoke = false;
    schedule_wakeup(app, control_flow, servo_smoke);
}

/// A `WaitUntil` deadline set by [`schedule_wakeup`] passed: blink cursors, advance the
/// loading sweep, expire status messages, poll focus and the `:res` monitor.
fn on_timer(app: &mut App) {
    // A transient status flash (e.g. a finished background `:ai`) times out.
    app.expire_status_flash();
    // Advance the tab strip's loading sweep. Mode-independent: a page can
    // load while you're typing a command or in passthrough.
    if app.any_tab_loading() {
        app.window.request_redraw();
    }
    // A held-back terminal resize (zoom/drag burst settling): repaint —
    // the draw's sync_active_term_size applies it once the target settles.
    if app.term_resize.deadline().is_some() {
        app.window.request_redraw();
    }
    // Repeatable pane-resize auto-leaves after a spell of no resize key, so a
    // later j/k (meant to scroll) doesn't silently resize.
    if app.mode == ModeKind::PaneResize
        && app.pane_resize_at.elapsed() >= crate::app::PANE_RESIZE_TIMEOUT
    {
        app.mode = ModeKind::Normal;
        app.clear_status();
        app.window.request_redraw();
    }
    if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
        app.cursor_on = !app.cursor_on;
        app.window.request_redraw();
    } else if matches!(
        app.mode,
        ModeKind::Normal | ModeKind::Scroll | ModeKind::ScrollCaret
    ) || (app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll)
    {
        // Read-mode caret: blink its block cursor.
        if app.read_caret_active() {
            app.cursor_on = !app.cursor_on;
            app.window.request_redraw();
        }
        // Focus backstop: reclaim keyboard focus if a click handed it to
        // the webview (see reclaim_focus_tick).
        app.reclaim_focus_tick();
        // Live `:res` monitor: re-sample on the tick.
        if app.active_is_res() {
            app.refresh_res();
            app.window.request_redraw();
        }
    }
}

fn on_window_event(app: &mut App, event: WindowEvent<'_>, control_flow: &mut ControlFlow) {
    match event {
        WindowEvent::CloseRequested => {
            app.teardown();
            *control_flow = ControlFlow::Exit;
        }
        WindowEvent::Resized(size) => {
            app.on_resize(size.width, size.height);
        }
        WindowEvent::ModifiersChanged(state) => {
            app.modifiers = state;
            app.on_modifiers_changed();
        }
        WindowEvent::Focused(focused) => {
            if focused {
                app.last_focus_gain = Instant::now();
            }
        }
        WindowEvent::CursorMoved { position, .. } => {
            app.cursor_pos = (position.x, position.y);
            // Left-drag inside the command line extends the selection.
            app.bar_drag(position.x);
            // Left-drag inside a terminal pane extends ITS selection.
            app.term_select_drag(position.x, position.y);
            // Hovering the command bar reveals the full (vs. shortened) URL.
            let (_, h) = app.inner();
            let over_bar = app.bar_h() > 0 && position.y >= (h as f64 - app.bar_h() as f64);
            if over_bar != app.bar_hover {
                app.bar_hover = over_bar;
                if app.mode == ModeKind::Normal {
                    app.window.request_redraw();
                }
            }
        }
        // The cursor left the parent client area — including moving up onto the
        // child webview, which swallows CursorMoved. Drop the bar hover so the
        // URL collapses back to its short form instead of getting stuck full.
        WindowEvent::CursorLeft { .. } => {
            // A release over the child webview can be missed by the parent, so
            // also end any in-progress drag-select here.
            app.bar_dragging = false;
            // Same for a terminal drag — but drop it WITHOUT copying: leaving
            // the client area (often just crossing onto a sibling web pane's
            // HWND) isn't a release, and shouldn't clobber the clipboard. What
            // was selected stays highlighted and yankable.
            app.term_drag = None;
            if app.bar_hover {
                app.bar_hover = false;
                if app.mode == ModeKind::Normal {
                    app.window.request_redraw();
                }
            }
        }
        // Clicks on the top tab bar: hit a tab label to switch to it, or
        // drag the borderless window by the empty strip (QoL — like a title
        // bar). The webview owns clicks below the bar.
        WindowEvent::MouseInput {
            state: ElementState::Released,
            button: MouseButton::Left,
            ..
        } => {
            app.bar_dragging = false;
            // Ends a terminal drag-select and copies what it covered.
            app.term_select_end();
        }
        // Right-press on a natively-drawn surface (terminal, :read, a vim
        // pager, the command line): copy that surface's selection. Web panes
        // handle their own right-click in the page's context menu.
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Right,
            ..
        } => app.right_click_copy(app.cursor_pos.0, app.cursor_pos.1),
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } => {
            let (_, h) = app.inner();
            let bar_top = h as f64 - app.bar_h() as f64;
            if app.bar_h() > 0 && app.cursor_pos.1 >= bar_top {
                // Click the command/status bar: edit the URL or place the caret.
                app.bar_click(app.cursor_pos.0);
            } else if !app.tabs.is_empty() && app.cursor_pos.1 < app.tab_bar_h() as f64 {
                if let Some(i) = app.tab_at_pixel(app.cursor_pos.0) {
                    app.jump_to(i);
                    app.window.request_redraw();
                } else {
                    let _ = app.window.drag_window();
                }
            } else if let Some((tab, rect)) = app.pane_at_pixel(app.cursor_pos.0, app.cursor_pos.1)
            {
                // A press on a (native) pane below the tab bar. Web panes
                // consume the click in their own HWND, so this only fires for
                // terminal/read/vim/blank panes; the web half of the same
                // gesture arrives as `PaneClick`. Both go through
                // `focus_pane_click`, which carries passthrough across the move
                // and hands the keyboard to whichever pane now owns it.
                if app.is_split() {
                    app.focus_pane_click(tab);
                }
                // Then start a drag-selection if that pane is a terminal (also
                // when unsplit — one terminal filling the band still selects).
                app.term_select_start(tab, rect, app.cursor_pos.0, app.cursor_pos.1);
            }
        }
        // Mouse wheel: scroll the native-drawn content under the cursor (the
        // terminal's scrollback, or a `:read` document). Web tabs receive the
        // wheel directly via their child window, so they never reach here.
        WindowEvent::MouseWheel { delta, .. } => {
            let dy = match delta {
                MouseScrollDelta::LineDelta(_, y) => y as f64,
                MouseScrollDelta::PixelDelta(pos) => pos.y / 40.0,
                _ => 0.0,
            };
            if dy != 0.0 {
                app.on_wheel(dy);
            }
        }
        WindowEvent::KeyboardInput { event: key, .. } if key.state == ElementState::Pressed => {
            app.handle_key(&key);
            if app.quit {
                app.teardown();
                *control_flow = ControlFlow::Exit;
            }
        }
        _ => {}
    }
}

fn on_user_event(app: &mut App, event: UserEvent, control_flow: &mut ControlFlow) {
    match event {
        // Already unwrapped and routed to its view at the top of `handle`.
        UserEvent::Engine { .. } => {}
        // Only a page sends it, and `route_engine_event` handles it for that page.
        UserEvent::HintClick(..) => {}
        // Consumed by `engines::servo::intercept` at the top of `handle`.
        #[cfg(all(windows, feature = "servo-engine"))]
        UserEvent::Servo(_) => {}
        UserEvent::ExitToNormal => app.exit_to_normal(),
        UserEvent::SyncAdblock => app.broadcast_adblock(),
        UserEvent::FocusShell => {
            match app.mode {
                ModeKind::Hint if app.hint_act == HintAct::Scroll => {
                    app.exit_hint();
                    app.reclaim_shell_focus();
                }
                // Passthrough persists across navigation: re-assert it on the new
                // page and keep the page focused.
                ModeKind::Passthrough => {
                    app.set_page_mode("passthrough");
                    if let Some(wv) = app.active_webview() {
                        let _ = wv.focus();
                    }
                }
                // Insert, Caret and Scroll are tied to the old page's DOM; navigation ends
                // them (the field/caret is gone on the new document).
                ModeKind::Insert | ModeKind::Caret | ModeKind::Scroll | ModeKind::ScrollCaret => {
                    app.set_page_mode("normal");
                    app.mode = ModeKind::Normal;
                    app.reclaim_shell_focus();
                    app.window.request_redraw();
                }
                // Reclaim KEYBOARD focus from the webview child, but never steal the
                // FOREGROUND: this fires on every page-load/`page-ready`, and a busy
                // site (ads, video, redirects) that finishes loading after you've
                // alt-tabbed away must not pop the window back to the front.
                // `SetFocus` no-ops when we aren't the foreground app, so leaving is
                // respected; the periodic `reclaim_focus_tick` restores keyboard
                // focus when you actually return.
                _ => app.reclaim_shell_focus(),
            }
            // A navigation (e.g. clicking a link that yielded focus) lands on a
            // fresh page with the shell back in control — end any page-focus yield.
            app.page_focus_yielded = false;
            // The old page's hovered-link readout is stale on a new document.
            app.hover_link = None;
            // A fresh navigation can reset the page's zoom factor — re-apply.
            app.apply_active_zoom();
            // Track the post-navigation URL in the status bar.
            app.refresh_active_url();
            app.window.request_redraw();
        }
        UserEvent::GrabFocus => {
            // Only in Normal mode: the shell owns the keyboard there. In
            // Insert/Passthrough the page legitimately holds focus. A bare-area
            // click ends any prior page-focus yield.
            if app.mode == ModeKind::Normal {
                app.page_focus_yielded = false;
                // The gesture resolved to "the shell keeps the keyboard": end the
                // grace so the poll guards this page again immediately.
                app.page_gesture_at = None;
                app.reclaim_shell_focus();
            }
        }
        // A click on a page control left focus in the page so its menu stays open.
        UserEvent::PageHold => {
            if app.mode == ModeKind::Normal && app.active_webview().is_some() {
                app.page_focus_yielded = true;
                app.window.request_redraw();
            }
        }
        // A click landed in a text field: enter Insert so typing goes to the page
        // (and clicking away later drops back to Normal on its own).
        UserEvent::PageEdit => {
            if app.mode == ModeKind::Normal {
                app.page_focus_yielded = false;
                app.enter_insert();
                app.window.request_redraw();
            }
        }
        UserEvent::LinkHover(href) => {
            // Only the focused tab's hover readout matters; ignore stray reports
            // from a background pane. Empty = the pointer left the link.
            let next = (!href.is_empty()).then_some(href);
            if app.hover_link != next {
                app.hover_link = next;
                app.window.request_redraw();
            }
        }
        UserEvent::ReclaimNormal => app.reclaim_from_page(),
        UserEvent::ReplayToShell(key) => {
            // Only while still in Normal: a key queued behind one that changed
            // mode (`:` opening the command bar) is already on its way to the
            // shell, which holds focus by then.
            if app.mode == ModeKind::Normal {
                app.reclaim_from_page();
                // Publish the mode now, so keys typed right behind this one see
                // the shell's new state.
                shellkeys::set_mode(app.key_mode());
            }
            shellkeys::replay(key);
        }
        UserEvent::PaneClick => {
            // A gesture is under way in the page: hold the focus-reclaim poll off
            // until it has finished and the bridge has said who should keep the
            // keyboard (see `reclaim_focus_tick`).
            app.page_gesture_at = Some(Instant::now());
            // Clicking inside a web pane focuses it (web panes consume the click in
            // their HWND, so this is the only way they reach us). Use the OS cursor
            // position, since CursorMoved isn't delivered over a child webview.
            // `focus_pane_click` only marks the pane active — it must NOT reclaim the
            // keyboard, or it'd yank focus straight back off the page you just
            // clicked, leaving the pane merely "selected" until a second click.
            if app.is_split() {
                if let Some((x, y)) = app.cursor_client_pos() {
                    if let Some((tab, _)) = app.pane_at_pixel(x as f64, y as f64) {
                        app.focus_pane_click(tab);
                    }
                }
            }
        }
        UserEvent::ExitHint => {
            app.hint_input.clear();
            app.hint_act = HintAct::Follow;
            app.mode = ModeKind::Normal;
            app.window.set_focus();
            app.window.request_redraw();
        }
        UserEvent::ScrollSelected => {
            if app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll {
                app.hint_input.clear();
                app.hint_act = HintAct::Follow;
                app.mode = ModeKind::Scroll;
                app.set_page_mode("scroll");
                app.reclaim_shell_focus();
                app.window.request_redraw();
            }
        }
        UserEvent::ScrollExit => {
            if matches!(app.mode, ModeKind::Scroll | ModeKind::ScrollCaret) {
                app.exit_to_normal();
                app.set_status("scroll target is no longer available");
            } else if app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll {
                app.exit_hint();
                app.set_status("no scrollable boxes on screen");
            }
        }
        UserEvent::HintEdit => {
            // The hint selected a text field: enter Insert (type into the page),
            // then focus the field itself within the page.
            app.hint_input.clear();
            app.enter_insert();
            if let Some(wv) = app.active_webview() {
                let _ = wv.evaluate_script(
                    "window.__hintTarget&&(window.__hintTarget.focus(),window.__hintTarget=null)",
                );
            }
            app.window.request_redraw();
        }
        UserEvent::HintOpen(url) => {
            // The page already cleared its badges; just reset shell hint state
            // and open the link in a fresh tab.
            app.hint_input.clear();
            app.hint_act = HintAct::Follow;
            app.mode = ModeKind::Normal;
            app.window.set_focus();
            app.open_tab(&url, app.nojs, true);
        }
        UserEvent::HintCopy(url) => {
            // Copy mode (`yf`): the page already cleared its badges — reset the
            // shell's hint state, take the keyboard back, and yank the address.
            app.hint_input.clear();
            app.hint_act = HintAct::Follow;
            app.mode = ModeKind::Normal;
            app.window.set_focus();
            app.copy_text(&url);
            app.window.request_redraw();
        }
        UserEvent::ReadReady {
            doc,
            replace,
            record,
        } => {
            app.show_read_document(*doc, replace, record);
        }
        UserEvent::ReadFailed(e) => {
            app.set_error(format!("read failed: {e}"));
            app.window.request_redraw();
        }
        UserEvent::Navigate(url) => {
            if let Some(i) = app.active {
                // Shell-driven `load_url` reads as not-user-initiated; stamp intent so
                // the native guard lets this de-proxy redirect through.
                if let Some(wv) = app.tabs.get(i).and_then(|t| t.webview()) {
                    let _ = wv.load_url(&url);
                }
                // Reflect the de-proxied address in the status bar right away
                // (the live URL refresh on page-load will confirm it).
                if let Some(t) = app.tabs.get_mut(i) {
                    t.url = url;
                }
                app.window.request_redraw();
            }
        }
        UserEvent::RedirectBlocked(url) => {
            // Show the destination we refused, truncated so a long tracking URL
            // doesn't blow out the status bar.
            let short: String = url.chars().take(80).collect();
            app.set_status(format!("blocked redirect → {short}"));
            app.window.request_redraw();
        }
        UserEvent::PopupBlocked(url) => {
            let short: String = url.chars().take(80).collect();
            app.set_status(format!("blocked pop-up → {short}  (:ads off to allow)"));
            app.window.request_redraw();
        }
        UserEvent::OpenPopupTab(url) => {
            // A real new-tab click the popup guard cleared: re-open it as a managed
            // tab (new tabs here live in our tab strip, not as OS popups).
            app.open_tab(&url, app.nojs, true);
        }
        UserEvent::BlocklistReady => {
            // Quiet by default (don't clobber a useful status); the engine simply
            // starts catching navigations from here on.
        }
        UserEvent::ExtensionsListed {
            request,
            view,
            result,
        } => {
            if request == app.extension_request && app.view_by_id(view).is_some() {
                match result {
                    Ok(items) => app.show_extensions_page(view, items),
                    Err(error) => app.set_error(error),
                }
            }
        }
        UserEvent::DownloadBlocked(name) => {
            let short: String = name.chars().take(60).collect();
            app.set_error(format!(
                "blocked download of {short} — executable/installer. :downloads to allow"
            ));
            app.window.request_redraw();
        }
        UserEvent::DataCleared {
            label,
            ai_id,
            result,
        } => {
            // Announce failures, including those from the bonus engine-history
            // clear. An empty label only suppresses successful bonus reports.
            if let Err(error) = result {
                let target = if label.is_empty() {
                    "engine history"
                } else {
                    &label
                };
                let message = format!("could not clear {target}: {error}");
                let shown_in_chat = ai_id.is_some_and(|id| app.ai_note(id, &message));
                if !shown_in_chat {
                    app.set_error(message);
                    app.window.request_redraw();
                }
            } else if !label.is_empty() {
                // If an :ai tab asked for it, confirm in that chat; fall back to the
                // status bar only when that tab isn't the one on screen.
                let shown_in_chat = ai_id.is_some_and(|id| app.ai_action_done(id, &label));
                if !shown_in_chat {
                    app.set_status(format!("cleared {label}"));
                    app.window.request_redraw();
                }
            }
        }
        UserEvent::SchemeInstalled { ai_id, result } => {
            app.finish_scheme_install(ai_id, result);
        }
        UserEvent::RestoreDefaults => {
            // The Ctrl+Alt+Shift+R panic chord, pressed in a focused page (WebView2's
            // accelerator event, ahead of the page). Same action as `:restore`.
            app.run_action("restore", serde_json::json!({}));
        }
        UserEvent::TermDone { cmd, output, code } => {
            app.show_term_result(&cmd, &output, code);
        }
        UserEvent::TermOutput { id, data } => app.feed_terminal(id, &data),
        UserEvent::CaretYank(text) => {
            let n = text.chars().count();
            clipboard_set(&text);
            if app.mode == ModeKind::Caret {
                app.mode = ModeKind::Normal;
            } else if app.mode == ModeKind::ScrollCaret {
                app.mode = ModeKind::Scroll;
            }
            app.set_status(format!("yanked {n} chars"));
            app.window.request_redraw();
        }
        UserEvent::ClipCopy(text) => {
            app.copy_text(&text);
            app.window.request_redraw();
        }
        UserEvent::CaretExit => {
            if matches!(app.mode, ModeKind::Caret | ModeKind::ScrollCaret) {
                app.mode = if app.mode == ModeKind::ScrollCaret {
                    ModeKind::Scroll
                } else {
                    ModeKind::Normal
                };
                app.clear_status();
                app.window.request_redraw();
            }
        }
        UserEvent::AiReply {
            id,
            convo,
            round,
            result,
        } => app.ai_reply(id, convo, round, result),
        UserEvent::PageFullscreen(on) => app.set_page_fullscreen(on),
        // An SPA navigation changed the page URL without a document load: sync the
        // stored URL (and, for real steps, the H/L back stack) + repaint the bar.
        UserEvent::UrlChanged { record } => {
            app.refresh_active_url_record(record);
            app.window.request_redraw();
        }
        UserEvent::Redraw => app.window.request_redraw(),
        UserEvent::TermClosed { id } => app.close_term_tab(id),
        UserEvent::Quit => {
            app.teardown();
            *control_flow = ControlFlow::Exit;
        }
    }
}

/// Decide when the loop next needs to wake: plain `Wait` unless something is animating,
/// polling or waiting on a deadline. `servo_smoke` is the Servo smoke test's tick flag
/// (always false without the `servo-engine` feature).
fn schedule_wakeup(app: &App, control_flow: &mut ControlFlow, servo_smoke: bool) {
    // While typing a command, keep waking to blink the cursor (unless we're
    // already exiting). Outside Command mode we stay on plain Wait.
    if !matches!(
        *control_flow,
        ControlFlow::Exit | ControlFlow::ExitWithCode(_)
    ) {
        if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
            // Blink the command-bar cursor.
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
        } else if app.mode == ModeKind::Normal && app.read_caret_active() {
            // Blink the read-mode caret's block cursor.
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
        } else if app.active_pane_is_webview
            && (matches!(
                app.mode,
                ModeKind::Normal | ModeKind::Scroll | ModeKind::ScrollCaret
            ) || (app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll))
        {
            // Poll to keep keyboard focus on the shell while the FOCUSED pane is a
            // web tab (the click-focus backstop).
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));
        } else if app.mode == ModeKind::Normal && app.background_webview_visible {
            // A web pane is merely visible beside a focused terminal/read pane: it
            // can still trap the keyboard on a stray click, but that's rare — tick
            // slowly so working in the native pane stays near-idle. Fully idle
            // otherwise — zero wakeups on welcome/read/term-only layouts.
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
        } else if app.mode == ModeKind::Normal && app.active_is_res() {
            // Auto-refresh the live resource monitor about once a second.
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
        } else if app.mode == ModeKind::PaneResize {
            // Wake at the resize-mode idle deadline so it can auto-exit.
            *control_flow =
                ControlFlow::WaitUntil(app.pane_resize_at + crate::app::PANE_RESIZE_TIMEOUT);
        }
        // A page is loading: wake fast enough for the tab strip's progress sweep to
        // read as motion. Merged (not chained onto the mode branches above) because
        // a load can be in flight in any mode.
        if app.any_tab_loading() {
            let deadline = Instant::now() + Duration::from_millis(60);
            let next = match *control_flow {
                ControlFlow::WaitUntil(t) => t.min(deadline),
                _ => deadline,
            };
            *control_flow = ControlFlow::WaitUntil(next);
        }
        // A pending status-flash auto-clear: wake at its deadline (or sooner, if
        // another timer above already wins).
        if let Some(clear_at) = app.status.clear_at() {
            let next = match *control_flow {
                ControlFlow::WaitUntil(t) => t.min(clear_at),
                _ => clear_at,
            };
            *control_flow = ControlFlow::WaitUntil(next);
        }
        // A held-back terminal resize: wake when its settle window closes.
        if let Some(deadline) = app.term_resize.deadline() {
            let next = match *control_flow {
                ControlFlow::WaitUntil(t) => t.min(deadline),
                _ => deadline,
            };
            *control_flow = ControlFlow::WaitUntil(next);
        }
        if servo_smoke {
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50));
        }
        #[cfg(all(windows, feature = "servo-engine"))]
        if let Some(deadline) = engines::servo::tick(app) {
            let next = match *control_flow {
                ControlFlow::WaitUntil(t) => t.min(deadline),
                _ => deadline,
            };
            *control_flow = ControlFlow::WaitUntil(next);
        }
    }
}

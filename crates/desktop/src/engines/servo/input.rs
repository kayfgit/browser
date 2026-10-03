use euclid::Scale;
use servo::{InputEvent, WebView};
use tao::{
    event::{ElementState, MouseScrollDelta, WindowEvent},
    window::Window,
};

/// Press and release the primary button at `(x, y)`, CSS pixels in the viewport.
/// Embedder input is real user input to the page (a user gesture), unlike a click
/// dispatched from script. Servo maps page coordinates through zoom itself.
pub(super) fn click(view: &WebView, x: f64, y: f64) -> Result<(), String> {
    if !x.is_finite() || !y.is_finite() {
        return Err("click position must be finite".into());
    }
    let point: servo::WebViewPoint =
        euclid::Point2D::<f32, servo::CSSPixel>::new(x as f32, y as f32).into();
    view.notify_input_event(InputEvent::MouseMove(servo::MouseMoveEvent::new(point)));
    for action in [servo::MouseButtonAction::Down, servo::MouseButtonAction::Up] {
        view.notify_input_event(InputEvent::MouseButton(servo::MouseButtonEvent::new(
            action,
            servo::MouseButton::Primary,
            point,
        )));
    }
    Ok(())
}

#[derive(Default)]
pub struct Input {
    point: servo::DevicePoint,
    modifiers: tao::keyboard::ModifiersState,
    keyboard_text: Option<String>,
}

impl Input {
    pub fn handle(&mut self, view: &WebView, window: &Window, event: WindowEvent<'_>) {
        match event {
            WindowEvent::Resized(size) => view.resize(dpi::PhysicalSize::new(
                size.width.max(1),
                size.height.max(1),
            )),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                view.set_hidpi_scale_factor(Scale::new(scale_factor as f32))
            }
            WindowEvent::Focused(true) => view.focus(),
            WindowEvent::Focused(false) => {
                self.modifiers = Default::default();
                self.keyboard_text = None;
                view.blur();
            }
            WindowEvent::ModifiersChanged(value) => self.modifiers = value,
            WindowEvent::CursorMoved { position, .. } => {
                self.point = servo::DevicePoint::new(position.x as f32, position.y as f32);
                view.notify_input_event(InputEvent::MouseMove(servo::MouseMoveEvent::new(
                    self.point.into(),
                )));
            }
            WindowEvent::CursorLeft { .. } => {
                view.notify_input_event(InputEvent::MouseLeftViewport(Default::default()));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed {
                    super::native::focus(window);
                    view.focus();
                }
                let button = match button {
                    tao::event::MouseButton::Left => servo::MouseButton::Primary,
                    tao::event::MouseButton::Right => servo::MouseButton::Secondary,
                    tao::event::MouseButton::Middle => servo::MouseButton::Auxiliary,
                    tao::event::MouseButton::Other(n) => servo::MouseButton::Other(n),
                    _ => return,
                };
                let action = if state == ElementState::Pressed {
                    servo::MouseButtonAction::Down
                } else {
                    servo::MouseButtonAction::Up
                };
                view.notify_input_event(InputEvent::MouseButton(servo::MouseButtonEvent::new(
                    action,
                    button,
                    self.point.into(),
                )));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y, mode) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (
                        f64::from(x) * super::native::wheel_line_pixels(window, true),
                        f64::from(y) * super::native::wheel_line_pixels(window, false),
                        servo::WheelMode::DeltaPixel,
                    ),
                    MouseScrollDelta::PixelDelta(p) => (p.x, p.y, servo::WheelMode::DeltaPixel),
                    _ => return,
                };
                view.notify_input_event(InputEvent::Wheel(servo::WheelEvent::new(
                    servo::WheelDelta { x, y, z: 0.0, mode },
                    self.point.into(),
                )));
            }
            WindowEvent::KeyboardInput { event, .. } => self.keyboard(view, &event),
            // Tao exposes committed text only; full preedit requires a native adapter.
            WindowEvent::ReceivedImeText(text) => {
                let keyboard_text = self.keyboard_text.take();
                if text.is_empty()
                    || text.chars().any(char::is_control)
                    || keyboard_text.as_deref() == Some(text.as_str())
                {
                    return;
                }
                for state in [servo::CompositionState::Start, servo::CompositionState::End] {
                    view.notify_input_event(InputEvent::Ime(servo::ImeEvent::Composition(
                        servo::CompositionEvent {
                            state,
                            data: if state == servo::CompositionState::Start {
                                String::new()
                            } else {
                                text.clone()
                            },
                        },
                    )));
                }
            }
            _ => {}
        }
    }

    pub(super) fn keyboard(&mut self, view: &WebView, event: &tao::event::KeyEvent) {
        let key = match &event.logical_key {
            tao::keyboard::Key::Character(text) => servo::Key::Character((*text).into()),
            tao::keyboard::Key::Space => servo::Key::Character(" ".into()),
            tao::keyboard::Key::Super => servo::Key::Named(servo::NamedKey::Meta),
            tao::keyboard::Key::Dead(_) => servo::Key::Named(servo::NamedKey::Dead),
            other => format!("{other:?}")
                .parse()
                .unwrap_or(servo::Key::Named(servo::NamedKey::Unidentified)),
        };
        // Tao 0.35 on Windows also emits ReceivedImeText for ordinary
        // WM_CHAR messages. Servo already inserts character keys, so
        // suppress only the matching follow-up commit, not real IME text.
        self.keyboard_text = match (&key, event.state) {
            (servo::Key::Character(text), ElementState::Pressed) => Some(text.clone()),
            _ => None,
        };
        let mut mods = servo::Modifiers::empty();
        mods.set(servo::Modifiers::SHIFT, self.modifiers.shift_key());
        mods.set(servo::Modifiers::CONTROL, self.modifiers.control_key());
        mods.set(servo::Modifiers::ALT, self.modifiers.alt_key());
        mods.set(servo::Modifiers::META, self.modifiers.super_key());
        let location = match event.location {
            tao::keyboard::KeyLocation::Left => servo::Location::Left,
            tao::keyboard::KeyLocation::Right => servo::Location::Right,
            tao::keyboard::KeyLocation::Numpad => servo::Location::Numpad,
            _ => servo::Location::Standard,
        };
        let code = match event.physical_key {
            tao::keyboard::KeyCode::SuperLeft => servo::Code::MetaLeft,
            tao::keyboard::KeyCode::SuperRight => servo::Code::MetaRight,
            other => format!("{other:?}")
                .parse()
                .unwrap_or(servo::Code::Unidentified),
        };
        view.notify_input_event(InputEvent::Keyboard(
            servo::KeyboardEvent::new_without_event(
                if event.state == ElementState::Pressed {
                    servo::KeyState::Down
                } else {
                    servo::KeyState::Up
                },
                key,
                code,
                location,
                mods,
                event.repeat,
                false,
            ),
        ));
    }
}

use crate::{
    action::Action,
    config::KeybindAction,
    grabs::{
        move_grab::MoveGrab,
        resize_grab::{ResizeEdge, ResizeSurfaceGrab},
    },
    state::Raven,
};
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, MouseButton, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent,
    },
    desktop::{WindowSurfaceType, layer_map_for_output},
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, Focus, GrabStartData as PointerGrabStartData, MotionEvent,
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        input_method::InputMethodSeat,
        shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer},
    },
};

impl Raven {
    pub fn handle_input_event<B: InputBackend>(&mut self, event: InputEvent<B>) {
        match event {
            InputEvent::Keyboard { event } => self.handle_keyboard_event::<B>(event),
            InputEvent::PointerMotion { event } => self.handle_pointer_motion::<B>(event),
            InputEvent::PointerMotionAbsolute { event } => {
                self.handle_pointer_motion_absolute::<B>(event)
            }
            InputEvent::PointerButton { event } => self.handle_pointer_button::<B>(event),
            InputEvent::PointerAxis { event } => self.handle_pointer_axis::<B>(event),
            _ => {}
        }
    }

    fn handle_keyboard_event<B: InputBackend>(&mut self, event: B::KeyboardKeyEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let time_msec = Event::time_msec(&event);
        let key_code = event.key_code();
        let key_state = event.state();

        let keyboard = self.seat.get_keyboard().expect("keyboard not initialized");

        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            let exclusive_surface = {
                let layers = layer_map_for_output(&output);
                [WlrLayer::Overlay, WlrLayer::Top]
                    .into_iter()
                    .find_map(|layer_kind| {
                        layers.layers_on(layer_kind).find_map(|layer| {
                            let is_exclusive = layer.cached_state().keyboard_interactivity
                                == KeyboardInteractivity::Exclusive;
                            if !is_exclusive || layers.layer_geometry(layer).is_none() {
                                return None;
                            }
                            Some(layer.wl_surface().clone())
                        })
                    })
            };

            if let Some(surface) = exclusive_surface {
                self.set_keyboard_focus(Some(surface), serial);
                keyboard.input::<(), _>(self, key_code, key_state, serial, time_msec, |_, _, _| {
                    FilterResult::Forward
                });
                return;
            }
        }

        keyboard.input::<(), _>(
            self,
            key_code,
            key_state,
            serial,
            time_msec,
            |state, modifiers, keysym_handle| {
                if key_state == KeyState::Pressed {
                    let keysym = keysym_handle.modified_sym();
                    if handle_keybinding(state, modifiers, keysym) {
                        return FilterResult::Intercept(());
                    }
                }
                FilterResult::Forward
            },
        );
    }

    fn handle_pointer_motion<B: InputBackend>(&mut self, event: B::PointerMotionEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let delta = (event.delta_x(), event.delta_y()).into();

        self.pointer_location += delta;
        self.clamp_pointer_location();

        let pointer = self.pointer();
        let under = self.surface_under_pointer();

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: self.pointer_location,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);

        if self.config.focus_follow_mouse {
            self.update_keyboard_focus(self.pointer_location, serial, false);
        }
    }

    fn handle_pointer_motion_absolute<B: InputBackend>(
        &mut self,
        event: B::PointerMotionAbsoluteEvent,
    ) {
        let output_geo = self
            .space
            .outputs()
            .next()
            .map(|output| self.space.output_geometry(output).unwrap());

        let Some(output_geo) = output_geo else { return };

        self.pointer_location = (
            event.x_transformed(output_geo.size.w),
            event.y_transformed(output_geo.size.h),
        )
            .into();

        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.pointer();
        let under = self.surface_under_pointer();

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: self.pointer_location,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);

        if self.config.focus_follow_mouse {
            self.update_keyboard_focus(self.pointer_location, serial, false);
        }
    }

    fn handle_pointer_button<B: InputBackend>(&mut self, event: B::PointerButtonEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button();
        let button_code = event.button_code();
        let button_state = event.state();
        let pointer = self.pointer();

        // Keep pointer focus in sync with current cursor location before sending button events.
        // This ensures layer-shell clients (e.g. waybar) receive clicks even when no prior
        // motion event updated the pointer target.
        if !pointer.is_grabbed() {
            let under = self.surface_under_pointer();
            pointer.motion(
                self,
                under,
                &MotionEvent {
                    location: self.pointer_location,
                    serial,
                    time: event.time_msec(),
                },
            );
            pointer.frame(self);
        }

        let keyboard = self.seat.get_keyboard().expect("keyboard not initialized");
        let modifiers = keyboard.modifier_state();
        let main_key_held = self.config.main_key.matches(&modifiers);
        let resize_modifier_held = main_key_held || modifiers.alt;

        if ButtonState::Pressed == button_state
            && button == Some(MouseButton::Left)
            && main_key_held
            && let Some((window, _)) = self.window_under_pointer()
            && !pointer.is_grabbed()
        {
            let location = self.pointer_location;

            let start_data = PointerGrabStartData {
                focus: None,
                button: button_code,
                location,
            };
            let initial_window_location = self.space.element_location(&window).unwrap();
            let grab = MoveGrab {
                start_data,
                window: window.clone(),
                initial_window_location,
                current_window_location: initial_window_location,
            };
            pointer.set_grab(self, grab, serial, Focus::Clear);
            self.space.raise_element(&window, true);
        }

        if ButtonState::Pressed == button_state
            && button == Some(MouseButton::Right)
            && resize_modifier_held
            && let Some((window, window_location)) = self.window_under_pointer()
            && !pointer.is_grabbed()
        {
            let location = self.pointer_location;
            let window_size = window.geometry().size;
            let local_pos = location - window_location.to_f64();
            let edges = resize_edges_from_local_point(local_pos, window_size.w, window_size.h);

            let start_data = PointerGrabStartData {
                focus: None,
                button: button_code,
                location,
            };
            let initial_window_rect = Rectangle::new(window_location, window_size);
            let grab = ResizeSurfaceGrab::start(start_data, window.clone(), edges, initial_window_rect);
            pointer.set_grab(self, grab, serial, Focus::Clear);
            self.space.raise_element(&window, true);
        }

        if ButtonState::Pressed == button_state {
            self.update_keyboard_focus(self.pointer_location, serial, true);
        }

        pointer.button(
            self,
            &ButtonEvent {
                button: button_code,
                state: button_state,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn update_keyboard_focus(
        &mut self,
        location: Point<f64, Logical>,
        serial: Serial,
        raise: bool,
    ) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let input_method = self.seat.input_method();

        if !self.pointer().is_grabbed()
            && (!keyboard.is_grabbed() || input_method.keyboard_grabbed())
        {
            tracing::trace!("Pointer and keyboard are not grabbed");
            // There's only one output as of now
            let Some(output) = self.space.outputs().next().cloned() else {
                return;
            };
            let Some(output_geo) = self.space.output_geometry(&output) else {
                return;
            };

            let layers = layer_map_for_output(&output);

            // Keep focus on top/overlay layer surfaces during pointer motion so
            // launchers (e.g. fuzzel) don't lose focus and close when the mouse moves.
            if !raise
                && let Some(focused_surface) = keyboard.current_focus()
                && let Some(layer) =
                    layers.layer_for_surface(&focused_surface, WindowSurfaceType::TOPLEVEL)
                && matches!(layer.layer(), WlrLayer::Overlay | WlrLayer::Top)
                && layer.can_receive_keyboard_focus()
                && layers.layer_geometry(layer).is_some()
            {
                return;
            }

            #[allow(clippy::collapsible_if)]
            if let Some(layer) = layers
                .layer_under(WlrLayer::Overlay, location - output_geo.loc.to_f64())
                .or_else(|| layers.layer_under(WlrLayer::Top, location - output_geo.loc.to_f64()))
            {
                if layer.can_receive_keyboard_focus() {
                    tracing::debug!(
                        namespace = layer.namespace(),
                        "Layer can receive keyboard focus"
                    );

                    if let Some(layer_geo) = layers.layer_geometry(layer) {
                        if let Some((_, _)) = layer.surface_under(
                            location - output_geo.loc.to_f64() - layer_geo.loc.to_f64(),
                            WindowSurfaceType::ALL,
                        ) {
                            let namespace = layer.namespace();
                            tracing::debug!(namespace, "Set keyboard focus for layer");
                            self.set_keyboard_focus(Some(layer.wl_surface().clone()), serial);
                            return;
                        }
                    }
                }
            }

            if let Some((window, _)) = self
                .space
                .element_under(location)
                .map(|(w, p)| (w.clone(), p))
            {
                tracing::debug!("Setting focus of surface under pointer");
                if raise {
                    self.space.raise_element(&window, true);
                }
                if let Some(toplevel) = window.toplevel() {
                    self.set_keyboard_focus(Some(toplevel.wl_surface().clone()), serial);
                    return;
                }
            }

            #[allow(clippy::collapsible_if)]
            if let Some(layer) = layers
                .layer_under(WlrLayer::Bottom, location - output_geo.loc.to_f64())
                .or_else(|| {
                    layers.layer_under(WlrLayer::Background, location - output_geo.loc.to_f64())
                })
            {
                if layer.can_receive_keyboard_focus() {
                    if let Some(layer_geo) = layers.layer_geometry(layer) {
                        if let Some((_, _)) = layer.surface_under(
                            location - output_geo.loc.to_f64() - layer_geo.loc.to_f64(),
                            WindowSurfaceType::ALL,
                        ) {
                            self.set_keyboard_focus(Some(layer.wl_surface().clone()), serial);
                        }
                    }
                }
            }
        }
    }

    fn handle_pointer_axis<B: InputBackend>(&mut self, event: B::PointerAxisEvent) {
        // Like button events, keep pointer focus current so layer surfaces receive scroll.
        let pointer_serial = SERIAL_COUNTER.next_serial();
        let pointer = self.pointer();
        if !pointer.is_grabbed() {
            let under = self.surface_under_pointer();
            pointer.motion(
                self,
                under,
                &MotionEvent {
                    location: self.pointer_location,
                    serial: pointer_serial,
                    time: event.time_msec(),
                },
            );
            pointer.frame(self);
        }

        let horizontal_amount = event
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 3.0 / 120.0);
        let vertical_amount = event
            .amount(Axis::Vertical)
            .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 3.0 / 120.0);
        let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
        let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

        let mut axis_frame = AxisFrame::new(event.time_msec()).source(event.source());

        if horizontal_amount != 0.0 {
            axis_frame = axis_frame.value(Axis::Horizontal, horizontal_amount);
            if let Some(discrete) = horizontal_amount_discrete {
                axis_frame = axis_frame.v120(Axis::Horizontal, discrete as i32);
            }
        }

        if vertical_amount != 0.0 {
            axis_frame = axis_frame.value(Axis::Vertical, vertical_amount);
            if let Some(discrete) = vertical_amount_discrete {
                axis_frame = axis_frame.v120(Axis::Vertical, discrete as i32);
            }
        }

        if event.source() == AxisSource::Finger {
            if event.amount(Axis::Horizontal) == Some(0.0) {
                axis_frame = axis_frame.stop(Axis::Horizontal);
            }
            if event.amount(Axis::Vertical) == Some(0.0) {
                axis_frame = axis_frame.stop(Axis::Vertical);
            }
        }

        pointer.axis(self, axis_frame);
        pointer.frame(self);
    }

    fn clamp_pointer_location(&mut self) {
        let output_geo = self
            .space
            .outputs()
            .next()
            .map(|output| self.space.output_geometry(output).unwrap());

        if let Some(output_geo) = output_geo {
            self.pointer_location.x = self
                .pointer_location
                .x
                .clamp(0.0, output_geo.size.w as f64 - 1.0);
            self.pointer_location.y = self
                .pointer_location
                .y
                .clamp(0.0, output_geo.size.h as f64 - 1.0);
        }
    }
}

fn handle_keybinding(state: &mut Raven, modifiers: &ModifiersState, keysym: Keysym) -> bool {
    if let Some(action) = state.config.keybind_action_for(modifiers, keysym) {
        execute_keybind_action(state, action);
        return true;
    }

    let main_key_held = state.config.main_key.matches(modifiers);
    if !main_key_held {
        return false;
    }

    if let Some(workspace_index) = workspace_from_keysym(keysym) {
        if modifiers.shift {
            state
                .move_focused_window_to_workspace(workspace_index)
                .map_err(|err| tracing::warn!("failed to move window to workspace: {err}"))
                .ok();
        } else {
            state
                .switch_workspace(workspace_index)
                .map_err(|err| tracing::warn!("failed to switch workspace: {err}"))
                .ok();
        }
        return true;
    }

    false
}

fn execute_keybind_action(state: &mut Raven, action: KeybindAction) {
    match action {
        KeybindAction::Exec(command) => state.spawn_command(&command),
        KeybindAction::Terminal => state.spawn_terminal(),
        KeybindAction::Launcher => state.spawn_launcher(),
        KeybindAction::CloseFocused => close_focused_window(state),
        KeybindAction::ToggleFullscreen => {
            state
                .toggle_fullscreen_focused_window()
                .map_err(|err| tracing::warn!("failed to toggle fullscreen: {err}"))
                .ok();
        }
        KeybindAction::ToggleFloating => {
            state
                .toggle_floating_focused_window()
                .map_err(|err| tracing::warn!("failed to toggle floating: {err}"))
                .ok();
        }
        KeybindAction::Quit => state.loop_signal.stop(),
        KeybindAction::FocusNext => Action::FocusNext.execute(state),
        KeybindAction::FocusPrevious => Action::FocusPrevious.execute(state),
        KeybindAction::ReloadConfig => {
            state
                .reload_config()
                .map_err(|err| tracing::warn!("failed to reload config: {err}"))
                .ok();
        }
        KeybindAction::SwitchWorkspace(workspace_index) => {
            state
                .switch_workspace(workspace_index)
                .map_err(|err| tracing::warn!("failed to switch workspace: {err}"))
                .ok();
        }
        KeybindAction::MoveFocusedToWorkspace(workspace_index) => {
            state
                .move_focused_window_to_workspace(workspace_index)
                .map_err(|err| tracing::warn!("failed to move window to workspace: {err}"))
                .ok();
        }
        KeybindAction::Unsupported(name) => {
            tracing::warn!("action `{name}` is not implemented yet");
        }
    }
}

fn close_focused_window(state: &mut Raven) {
    let keyboard = state.seat.get_keyboard().unwrap();
    if let Some(focused_surface) = keyboard.current_focus()
        && let Some(window) = state.window_for_surface(&focused_surface)
    {
        tracing::info!("Closing focused window");
        window.toplevel().unwrap().send_close();
    }
}

fn workspace_from_keysym(keysym: Keysym) -> Option<usize> {
    match keysym {
        Keysym::_1 | Keysym::exclam => Some(0),
        Keysym::_2 | Keysym::at => Some(1),
        Keysym::_3 | Keysym::numbersign => Some(2),
        Keysym::_4 | Keysym::dollar => Some(3),
        Keysym::_5 | Keysym::percent => Some(4),
        Keysym::_6 | Keysym::asciicircum => Some(5),
        Keysym::_7 | Keysym::ampersand => Some(6),
        Keysym::_8 | Keysym::asterisk => Some(7),
        Keysym::_9 | Keysym::parenleft => Some(8),
        Keysym::_0 | Keysym::parenright => Some(9),
        _ => None,
    }
}

fn resize_edges_from_local_point(local: Point<f64, Logical>, width: i32, height: i32) -> ResizeEdge {
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;

    let horizontal = if local.x < width / 2.0 {
        ResizeEdge::LEFT
    } else {
        ResizeEdge::RIGHT
    };
    let vertical = if local.y < height / 2.0 {
        ResizeEdge::TOP
    } else {
        ResizeEdge::BOTTOM
    };

    horizontal | vertical
}

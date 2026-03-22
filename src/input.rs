use crate::{
    action::Action,
    config::KeybindAction,
    grabs::{
        move_grab::MoveGrab,
        resize_grab::{ResizeEdge, ResizeSurfaceGrab},
    },
    state::{PointContents, Raven},
};
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, MouseButton, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent,
    },
    desktop::{Window, WindowSurfaceType, layer_map_for_output},
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, Focus, GrabStartData as PointerGrabStartData, MotionEvent,
            PointerHandle, RelativeMotionEvent,
        },
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        input_method::InputMethodSeat,
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
        shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer},
    },
};

impl Raven {
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.workspace_windows()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|tl| tl.wl_surface() == surface)
            })
            .cloned()
            .or_else(|| {
                self.space
                    .elements()
                    .find(|window| {
                        window
                            .toplevel()
                            .is_some_and(|tl| tl.wl_surface() == surface)
                    })
                    .cloned()
            })
    }

    pub fn window_under_pointer(&self) -> Option<(Window, Point<i32, Logical>)> {
        self.space
            .element_under(self.pointer_location)
            .map(|(window, point)| (window.clone(), point))
    }

    pub fn contents_under(&self, position: Point<f64, Logical>) -> PointContents {
        let Some(output) = self.space.output_under(position).next() else {
            return PointContents::default();
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return PointContents::default();
        };

        let layer_map = layer_map_for_output(output);
        let position_within_output = position - output_geo.loc.to_f64();
        let fullscreen_on_output = self.output_has_fullscreen_window(output);

        let layer_surface_under = |layer: WlrLayer, popup: bool| -> Option<PointContents> {
            layer_map.layers_on(layer).rev().find_map(|layer_surface| {
                let layer_geo = layer_map.layer_geometry(layer_surface)?;
                let surface_type = (if popup {
                    WindowSurfaceType::POPUP
                } else {
                    WindowSurfaceType::TOPLEVEL
                }) | WindowSurfaceType::SUBSURFACE;

                layer_surface
                    .surface_under(
                        position_within_output - layer_geo.loc.to_f64(),
                        surface_type,
                    )
                    .map(|(surface, local_pos)| PointContents {
                        output: Some(output.clone()),
                        surface: Some((
                            surface,
                            output_geo.loc.to_f64() + layer_geo.loc.to_f64() + local_pos.to_f64(),
                        )),
                        window: None,
                        layer: Some(layer_surface.wl_surface().clone()),
                    })
            })
        };

        let window_under = || -> Option<PointContents> {
            self.space
                .element_under(position)
                .and_then(|(window, render_location)| {
                    window
                        .surface_under(position - render_location.to_f64(), WindowSurfaceType::ALL)
                        .map(|(surface, local_pos)| PointContents {
                            output: Some(output.clone()),
                            surface: Some((surface, (local_pos + render_location).to_f64())),
                            window: Some(window.clone()),
                            layer: None,
                        })
                })
        };

        if fullscreen_on_output {
            layer_surface_under(WlrLayer::Overlay, true)
                .or_else(|| layer_surface_under(WlrLayer::Overlay, false))
                .or_else(window_under)
                .or_else(|| layer_surface_under(WlrLayer::Top, true))
                .or_else(|| layer_surface_under(WlrLayer::Top, false))
                .or_else(|| layer_surface_under(WlrLayer::Bottom, true))
                .or_else(|| layer_surface_under(WlrLayer::Background, true))
                .or_else(|| layer_surface_under(WlrLayer::Bottom, false))
                .or_else(|| layer_surface_under(WlrLayer::Background, false))
                .unwrap_or_else(|| PointContents {
                    output: Some(output.clone()),
                    surface: None,
                    window: None,
                    layer: None,
                })
        } else {
            layer_surface_under(WlrLayer::Overlay, true)
                .or_else(|| layer_surface_under(WlrLayer::Overlay, false))
                .or_else(|| layer_surface_under(WlrLayer::Top, true))
                .or_else(|| layer_surface_under(WlrLayer::Top, false))
                .or_else(window_under)
                .or_else(|| layer_surface_under(WlrLayer::Bottom, true))
                .or_else(|| layer_surface_under(WlrLayer::Background, true))
                .or_else(|| layer_surface_under(WlrLayer::Bottom, false))
                .or_else(|| layer_surface_under(WlrLayer::Background, false))
                .unwrap_or_else(|| PointContents {
                    output: Some(output.clone()),
                    surface: None,
                    window: None,
                    layer: None,
                })
        }
    }

    pub fn update_pointer_contents(&mut self, time_msec: u32) -> bool {
        let pointer = self.pointer();
        let location = pointer.current_location();
        self.pointer_location = location;
        let under = self.contents_under(location);
        if self.pointer_contents == under {
            return false;
        }

        self.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: time_msec,
            },
        );
        self.maybe_activate_pointer_constraint();

        true
    }

    pub fn refresh_pointer_contents(&mut self) -> bool {
        let time_msec = self.start_time.elapsed().as_millis() as u32;
        if !self.update_pointer_contents(time_msec) {
            return false;
        }

        self.pointer().frame(self);
        self.queue_redraw_for_pointer_output();
        true
    }

    pub fn queue_redraw_for_pointer_output(&mut self) {
        let output = self.pointer_contents.output.clone().or_else(|| {
            self.space
                .output_under(self.pointer_location)
                .next()
                .cloned()
        });

        if let Some(output) = output {
            crate::backend::udev::queue_redraw_for_output(self, &output);
        } else {
            crate::backend::udev::queue_redraw_all(self);
        }
    }

    pub fn maybe_activate_pointer_constraint(&self) {
        let Some((surface, surface_loc)) = &self.pointer_contents.surface else {
            return;
        };

        let pointer = self.pointer();
        if Some(surface) != pointer.current_focus().as_ref() {
            return;
        }

        smithay::wayland::pointer_constraints::with_pointer_constraint(
            surface,
            &pointer,
            |constraint| {
                let Some(constraint) = constraint else { return };

                if constraint.is_active() {
                    return;
                }

                if let Some(region) = constraint.region() {
                    let pointer_pos = pointer.current_location();
                    let pos_within_surface = pointer_pos - *surface_loc;
                    if !region.contains(pos_within_surface.to_i32_round()) {
                        return;
                    }
                }

                constraint.activate();
            },
        );
    }

    pub fn pointer(&self) -> PointerHandle<Self> {
        self.seat.get_pointer().expect("pointer not initialized")
    }

    pub fn sync_window_activation(&self, focused_window: Option<&Window>) {
        let windows: Vec<Window> = self.space.elements().cloned().collect();
        for window in windows {
            let is_focused = focused_window.is_some_and(|focused| focused == &window);
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    if is_focused {
                        state.states.set(xdg_toplevel::State::Activated);
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
        }
    }

    pub fn set_keyboard_focus(&mut self, target: Option<WlSurface>, serial: Serial) {
        let current_focus = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus());
        if current_focus.as_ref() == target.as_ref() {
            return;
        }

        let focused_window = target
            .as_ref()
            .and_then(|surface| self.window_for_surface(surface));
        if let Some(window) = focused_window.as_ref()
            && self.is_window_mapped(window)
        {
            self.raise_window_preserving_layer(window);
        }
        self.sync_window_activation(focused_window.as_ref());

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, target, serial);
        }
    }

    pub fn refocus_visible_window(&mut self) {
        if let Some(focused_surface) = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            && let Some(window) = self.window_for_surface(&focused_surface)
            && self.is_window_mapped(&window)
        {
            self.sync_window_activation(Some(&window));
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let pointer_target = self.window_under_pointer().and_then(|(window, _)| {
            window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface().clone())
        });

        let fallback_target = self.space.elements().last().and_then(|window| {
            window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface().clone())
        });

        let target = pointer_target.or(fallback_target);
        self.set_keyboard_focus(target, serial);
    }

    fn queue_pointer_redraw_throttled(&mut self, event_time_msec: u32) {
        // Avoid flooding redraw requests during high-rate mouse motion.
        // 8ms ~= 125 FPS, good enough for cursor smoothness while reducing stalls.
        const POINTER_REDRAW_MIN_DELTA_MS: u32 = 8;
        let should_redraw = self
            .last_pointer_redraw_msec
            .map(|last| event_time_msec.wrapping_sub(last) >= POINTER_REDRAW_MIN_DELTA_MS)
            .unwrap_or(true);

        if should_redraw {
            self.last_pointer_redraw_msec = Some(event_time_msec);
            self.queue_redraw_for_pointer_output();
        }
    }

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
                crate::backend::udev::queue_redraw_all(self);
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

        self.queue_pointer_redraw_throttled(event.time_msec());
    }

    fn handle_pointer_motion<B: InputBackend>(&mut self, event: B::PointerMotionEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let delta = (event.delta_x(), event.delta_y()).into();

        self.pointer_location += delta;
        self.clamp_pointer_location();

        let pointer = self.pointer();

        // Check if we have an active pointer constraint (locked or confined pointer).
        // This is used by games like Steam to lock the pointer for FPS-style mouse control.
        let mut pointer_confined = None;
        if let Some(under) = &self.pointer_contents.surface {
            let pos_within_surface = self.pointer_location - under.1;

            let mut pointer_locked = false;
            with_pointer_constraint(&under.0, &pointer, |constraint| {
                let Some(constraint) = constraint else { return };
                if !constraint.is_active() {
                    return;
                }

                // Constraint does not apply if not within region.
                if let Some(region) = constraint.region() {
                    if !region.contains(pos_within_surface.to_i32_round()) {
                        return;
                    }
                }

                match &*constraint {
                    PointerConstraint::Locked(_locked) => {
                        pointer_locked = true;
                    }
                    PointerConstraint::Confined(confine) => {
                        pointer_confined = Some((under.clone(), confine.region().cloned()));
                    }
                }
            });

            // If the pointer is locked, only send relative motion and don't change focus.
            // This is critical for games that lock the pointer for mouse-look controls.
            if pointer_locked {
                pointer.relative_motion(
                    self,
                    Some(under.clone()),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                pointer.frame(self);

                self.queue_pointer_redraw_throttled(event.time_msec());
                return;
            }
        }

        let under = self.contents_under(self.pointer_location);

        // Handle confined pointer - prevent pointer from leaving the surface.
        if let Some((focus_surface, region)) = pointer_confined {
            let mut prevent = false;

            // Prevent the pointer from leaving the focused surface.
            if Some(&focus_surface.0) != under.surface.as_ref().map(|(s, _)| s) {
                prevent = true;
            }

            // Prevent the pointer from leaving the confine region, if any.
            if let Some(region) = region {
                let new_pos_within_surface = self.pointer_location - focus_surface.1;
                if !region.contains(new_pos_within_surface.to_i32_round()) {
                    prevent = true;
                }
            }

            if prevent {
                pointer.relative_motion(
                    self,
                    Some(focus_surface),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                pointer.frame(self);

                return;
            }
        }

        self.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface.clone(),
            &MotionEvent {
                location: self.pointer_location,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.relative_motion(
            self,
            under.surface,
            &RelativeMotionEvent {
                delta: event.delta(),
                delta_unaccel: event.delta_unaccel(),
                utime: event.time(),
            },
        );
        pointer.frame(self);

        // Activate a new confinement if necessary.
        self.maybe_activate_pointer_constraint();

        if self.config.focus_follow_mouse {
            self.update_keyboard_focus(self.pointer_location, serial, false);
        }

        self.queue_pointer_redraw_throttled(event.time_msec());
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
        let under = self.contents_under(self.pointer_location);
        self.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location: self.pointer_location,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(self);

        // Activate pointer constraint if necessary (for games that lock the pointer).
        self.maybe_activate_pointer_constraint();

        if self.config.focus_follow_mouse {
            self.update_keyboard_focus(self.pointer_location, serial, false);
        }

        self.queue_redraw_for_pointer_output();
    }

    fn handle_pointer_button<B: InputBackend>(&mut self, event: B::PointerButtonEvent) {
        let serial = SERIAL_COUNTER.next_serial();
        let button = event.button();
        let button_code = event.button_code();
        let button_state = event.state();
        let pointer = self.pointer();

        self.update_pointer_contents(event.time_msec());

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
            self.raise_window_preserving_layer(&window);
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
            let grab =
                ResizeSurfaceGrab::start(start_data, window.clone(), edges, initial_window_rect);
            pointer.set_grab(self, grab, serial, Focus::Clear);
            self.raise_window_preserving_layer(&window);
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

        self.queue_redraw_for_pointer_output();
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
                tracing::trace!("Setting focus of surface under pointer");
                if raise {
                    self.raise_window_preserving_layer(&window);
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
        let pointer = self.pointer();

        self.update_pointer_contents(event.time_msec());

        let horizontal_amount = event
            .amount(Axis::Horizontal)
            .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
        let vertical_amount = event
            .amount(Axis::Vertical)
            .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);
        let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
        let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

        let mut axis_frame = AxisFrame::new(event.time_msec()).source(event.source());

        if horizontal_amount != 0.0 {
            axis_frame = axis_frame
                .relative_direction(Axis::Horizontal, event.relative_direction(Axis::Horizontal))
                .value(Axis::Horizontal, horizontal_amount);
            if let Some(discrete) = horizontal_amount_discrete {
                axis_frame = axis_frame.v120(Axis::Horizontal, discrete as i32);
            }
        }

        if vertical_amount != 0.0 {
            axis_frame = axis_frame
                .relative_direction(Axis::Vertical, event.relative_direction(Axis::Vertical))
                .value(Axis::Vertical, vertical_amount);
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

        // Activate pointer constraint if necessary (for games that lock the pointer).
        self.maybe_activate_pointer_constraint();

        self.queue_redraw_for_pointer_output();
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
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_close();
        } else {
            #[cfg(feature = "xwayland")]
            if let Some(x11) = window.x11_surface() {
                let _ = x11.close();
            }
        }
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

fn resize_edges_from_local_point(
    local: Point<f64, Logical>,
    width: i32,
    height: i32,
) -> ResizeEdge {
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

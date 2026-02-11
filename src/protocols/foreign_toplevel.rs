use std::collections::{HashMap, HashSet, hash_map::Entry};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::{ToplevelCachedState, ToplevelStateSet, XdgToplevelSurfaceData};

use crate::Raven;

use zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1;
use zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;

const VERSION: u32 = 3;

pub struct ForeignToplevelManagerState {
    display: DisplayHandle,
    instances: Vec<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<WlSurface, ToplevelData>,
}

pub trait ForeignToplevelHandler {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState;
    fn activate(&mut self, wl_surface: WlSurface);
    fn close(&mut self, wl_surface: WlSurface);
    fn set_fullscreen(&mut self, wl_surface: WlSurface, wl_output: Option<WlOutput>);
    fn unset_fullscreen(&mut self, wl_surface: WlSurface);
    fn set_maximized(&mut self, wl_surface: WlSurface);
    fn unset_maximized(&mut self, wl_surface: WlSurface);
}

struct ToplevelData {
    title: Option<String>,
    app_id: Option<String>,
    states: Vec<u32>,
    output: Option<Output>,
    instances: HashMap<ZwlrForeignToplevelHandleV1, Vec<WlOutput>>,
}

pub struct ForeignToplevelGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl ForeignToplevelManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>,
        D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ForeignToplevelGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwlrForeignToplevelManagerV1, _>(VERSION, global_data);
        Self {
            display: display.clone(),
            instances: Vec::new(),
            toplevels: HashMap::new(),
        }
    }
}

pub fn refresh(state: &mut Raven) {
    let focused_surface = state
        .seat
        .get_keyboard()
        .and_then(|keyboard| keyboard.current_focus());

    let mut seen_windows = HashSet::new();
    let mut windows = Vec::new();

    for window in state
        .workspaces
        .iter()
        .flatten()
        .chain(state.space.elements())
    {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        let surface = toplevel.wl_surface();
        if seen_windows.insert(surface.clone()) {
            windows.push(window.clone());
        }
    }

    let live_surfaces = seen_windows;
    let protocol_state = &mut state.foreign_toplevel_manager_state;

    protocol_state.toplevels.retain(|surface, data| {
        if live_surfaces.contains(surface) {
            return true;
        }

        for instance in data.instances.keys() {
            instance.closed();
        }

        false
    });

    for window in windows {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        let wl_surface = toplevel.wl_surface().clone();
        let mapped = state.space.elements().any(|candidate| candidate == &window);
        let output = if mapped {
            state.space.outputs().next().cloned()
        } else {
            None
        };
        let has_focus = focused_surface.as_ref() == Some(&wl_surface);

        let (title, app_id, xdg_states) = with_states(&wl_surface, |states| {
            let role = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("xdg toplevel role data missing")
                .lock()
                .expect("xdg toplevel role lock poisoned");
            let mut cached = states.cached_state.get::<ToplevelCachedState>();
            let current_state = cached
                .current()
                .last_acked
                .as_ref()
                .map(|c| c.state.clone());
            (role.title.clone(), role.app_id.clone(), current_state)
        });

        let states = to_state_vec(xdg_states.as_ref().map(|state| &state.states), has_focus);
        refresh_toplevel(protocol_state, &wl_surface, title, app_id, states, output);
    }
}

pub fn on_output_bound(state: &mut Raven, output: &Output, wl_output: &WlOutput) {
    let Some(client) = wl_output.client() else {
        return;
    };

    let protocol_state = &mut state.foreign_toplevel_manager_state;
    for data in protocol_state.toplevels.values_mut() {
        if data.output.as_ref() != Some(output) {
            continue;
        }

        for (instance, outputs) in &mut data.instances {
            if instance.client().as_ref() != Some(&client) {
                continue;
            }

            instance.output_enter(wl_output);
            instance.done();
            outputs.push(wl_output.clone());
        }
    }
}

fn refresh_toplevel(
    protocol_state: &mut ForeignToplevelManagerState,
    wl_surface: &WlSurface,
    title: Option<String>,
    app_id: Option<String>,
    states: Vec<u32>,
    output: Option<Output>,
) {
    match protocol_state.toplevels.entry(wl_surface.clone()) {
        Entry::Occupied(entry) => {
            let data = entry.into_mut();

            let mut title_changed = false;
            if data.title != title {
                data.title = title;
                title_changed = true;
            }

            let mut app_id_changed = false;
            if data.app_id != app_id {
                data.app_id = app_id;
                app_id_changed = true;
            }

            let mut states_changed = false;
            if data.states != states {
                data.states = states;
                states_changed = true;
            }

            let mut output_changed = false;
            if data.output.as_ref() != output.as_ref() {
                data.output = output;
                output_changed = true;
            }

            if title_changed || app_id_changed || states_changed || output_changed {
                for (instance, outputs) in &mut data.instances {
                    if title_changed && let Some(title) = &data.title {
                        instance.title(title.clone());
                    }
                    if app_id_changed && let Some(app_id) = &data.app_id {
                        instance.app_id(app_id.clone());
                    }
                    if states_changed {
                        instance.state(data.states.iter().flat_map(|x| x.to_ne_bytes()).collect());
                    }
                    if output_changed {
                        for wl_output in outputs.drain(..) {
                            instance.output_leave(&wl_output);
                        }
                        if let Some(output) = &data.output
                            && let Some(client) = instance.client()
                        {
                            for wl_output in output.client_outputs(&client) {
                                instance.output_enter(&wl_output);
                                outputs.push(wl_output);
                            }
                        }
                    }
                    instance.done();
                }
            }

            for outputs in data.instances.values_mut() {
                outputs.retain(|output| output.is_alive());
            }
        }
        Entry::Vacant(entry) => {
            let mut data = ToplevelData {
                title,
                app_id,
                states,
                output,
                instances: HashMap::new(),
            };

            for manager in &protocol_state.instances {
                if let Some(client) = manager.client() {
                    data.add_instance::<Raven>(&protocol_state.display, &client, manager);
                }
            }

            entry.insert(data);
        }
    }
}

impl ToplevelData {
    fn add_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
    ) where
        D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
        D: 'static,
    {
        let toplevel = client
            .create_resource::<ZwlrForeignToplevelHandleV1, _, D>(handle, manager.version(), ())
            .expect("failed to create foreign toplevel handle");
        manager.toplevel(&toplevel);

        if let Some(title) = &self.title {
            toplevel.title(title.clone());
        }
        if let Some(app_id) = &self.app_id {
            toplevel.app_id(app_id.clone());
        }

        toplevel.state(self.states.iter().flat_map(|x| x.to_ne_bytes()).collect());

        let mut outputs = Vec::new();
        if let Some(output) = &self.output {
            for wl_output in output.client_outputs(client) {
                toplevel.output_enter(&wl_output);
                outputs.push(wl_output);
            }
        }

        toplevel.done();
        self.instances.insert(toplevel, outputs);
    }
}

impl<D> GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData, D>
    for ForeignToplevelManagerState
where
    D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>,
    D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
    D: ForeignToplevelHandler,
{
    fn bind(
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());

        let protocol_state = state.foreign_toplevel_manager_state();
        for data in protocol_state.toplevels.values_mut() {
            data.add_instance::<D>(handle, client, &manager);
        }

        protocol_state.instances.push(manager);
    }

    fn can_view(client: Client, global_data: &ForeignToplevelGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<ZwlrForeignToplevelManagerV1, (), D> for ForeignToplevelManagerState
where
    D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
    D: ForeignToplevelHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: <ZwlrForeignToplevelManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            resource.finished();
            let protocol_state = state.foreign_toplevel_manager_state();
            protocol_state
                .instances
                .retain(|instance| instance != resource);
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        let protocol_state = state.foreign_toplevel_manager_state();
        protocol_state
            .instances
            .retain(|instance| instance != resource);
    }
}

impl<D> Dispatch<ZwlrForeignToplevelHandleV1, (), D> for ForeignToplevelManagerState
where
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
    D: ForeignToplevelHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: <ZwlrForeignToplevelHandleV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let surface = {
            let protocol_state = state.foreign_toplevel_manager_state();
            protocol_state.toplevels.iter().find_map(|(surface, data)| {
                data.instances
                    .contains_key(resource)
                    .then(|| surface.clone())
            })
        };

        let Some(surface) = surface else {
            return;
        };

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => state.set_maximized(surface),
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.unset_maximized(surface)
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {}
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {}
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => state.activate(surface),
            zwlr_foreign_toplevel_handle_v1::Request::Close => state.close(surface),
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output } => {
                state.set_fullscreen(surface, output);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.unset_fullscreen(surface);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        _data: &(),
    ) {
        let protocol_state = state.foreign_toplevel_manager_state();
        for data in protocol_state.toplevels.values_mut() {
            data.instances.retain(|instance, _| instance != resource);
        }
    }
}

fn to_state_vec(states: Option<&ToplevelStateSet>, has_focus: bool) -> Vec<u32> {
    let mut result = Vec::with_capacity(3);
    if states.is_some_and(|s| s.contains(xdg_toplevel::State::Maximized)) {
        result.push(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32);
    }
    if states.is_some_and(|s| s.contains(xdg_toplevel::State::Fullscreen)) {
        result.push(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32);
    }
    if has_focus {
        result.push(zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
    }
    result
}

#[macro_export]
macro_rules! delegate_foreign_toplevel {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: $crate::protocols::foreign_toplevel::ForeignToplevelGlobalData
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: ()
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1: ()
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
    };
}

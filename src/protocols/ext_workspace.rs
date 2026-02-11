use std::collections::{HashMap, hash_map::Entry};

use ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1;
use ext_workspace_handle_v1::ExtWorkspaceHandleV1;
use ext_workspace_manager_v1::ExtWorkspaceManagerV1;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1, ext_workspace_handle_v1, ext_workspace_manager_v1,
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::state::{Raven, WORKSPACE_COUNT};

const VERSION: u32 = 1;

pub trait ExtWorkspaceHandler {
    fn ext_workspace_manager_state(&mut self) -> &mut ExtWorkspaceManagerState;
    fn activate_workspace(&mut self, workspace_index: usize);
}

enum Action {
    Activate(usize),
}

pub struct ExtWorkspaceManagerState {
    display: DisplayHandle,
    instances: HashMap<ExtWorkspaceManagerV1, Vec<Action>>,
    workspace_groups: HashMap<Output, WorkspaceGroupData>,
    workspaces: HashMap<usize, WorkspaceData>,
}

struct WorkspaceGroupData {
    instances: Vec<ExtWorkspaceGroupHandleV1>,
}

struct WorkspaceData {
    id: String,
    name: String,
    coordinates: [u32; 2],
    state: ext_workspace_handle_v1::State,
    output: Option<Output>,
    instances: Vec<ExtWorkspaceHandleV1>,
}

pub struct ExtWorkspaceGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub fn refresh(state: &mut Raven) {
    let outputs: Vec<Output> = state.space.outputs().cloned().collect();
    let primary_output = outputs.first().cloned();

    let protocol_state = &mut state.ext_workspace_manager_state;
    let mut changed = false;

    protocol_state
        .workspace_groups
        .retain(|output, group_data| {
            if outputs.iter().any(|candidate| candidate == output) {
                return true;
            }

            // Output disappeared: tell clients the group is gone.
            for group in &group_data.instances {
                group.removed();
            }
            changed = true;
            false
        });

    for output in &outputs {
        changed |= refresh_workspace_group(protocol_state, output);
    }

    for index in 0..WORKSPACE_COUNT {
        changed |= refresh_workspace(
            protocol_state,
            index,
            index == state.current_workspace,
            primary_output.as_ref(),
        );
    }

    let stale_workspaces: Vec<usize> = protocol_state
        .workspaces
        .keys()
        .copied()
        .filter(|index| *index >= WORKSPACE_COUNT)
        .collect();
    for index in stale_workspaces {
        if let Some(workspace) = protocol_state.workspaces.remove(&index) {
            remove_workspace_instances(&protocol_state.workspace_groups, &workspace);
            changed = true;
        }
    }

    if changed {
        for manager in protocol_state.instances.keys() {
            manager.done();
        }
    }
}

pub fn on_output_bound(state: &mut Raven, output: &Output, wl_output: &WlOutput) {
    let Some(client) = wl_output.client() else {
        return;
    };

    let mut sent = false;
    let protocol_state = &mut state.ext_workspace_manager_state;
    if let Some(group_data) = protocol_state.workspace_groups.get_mut(output) {
        for group in &mut group_data.instances {
            if group.client().as_ref() != Some(&client) {
                continue;
            }

            group.output_enter(wl_output);
            sent = true;
        }
    }

    if !sent {
        return;
    }

    for manager in protocol_state.instances.keys() {
        if manager.client().as_ref() == Some(&client) {
            manager.done();
        }
    }
}

fn refresh_workspace_group(protocol_state: &mut ExtWorkspaceManagerState, output: &Output) -> bool {
    if protocol_state.workspace_groups.contains_key(output) {
        return false;
    }

    let mut group_data = WorkspaceGroupData {
        instances: Vec::new(),
    };

    for manager in protocol_state.instances.keys() {
        if let Some(client) = manager.client() {
            group_data.add_instance::<Raven>(&protocol_state.display, &client, manager, output);
        }
    }

    // Send workspace_enter for already-known workspaces on this output.
    for group in &group_data.instances {
        let manager: &ExtWorkspaceManagerV1 = group.data().expect("missing group manager data");
        for workspace_data in protocol_state.workspaces.values() {
            if workspace_data.output.as_ref() != Some(output) {
                continue;
            }
            for workspace in &workspace_data.instances {
                if workspace.data() == Some(manager) {
                    group.workspace_enter(workspace);
                }
            }
        }
    }

    protocol_state
        .workspace_groups
        .insert(output.clone(), group_data);
    true
}

fn build_workspace_name(index: usize) -> String {
    (index + 1).to_string()
}

fn refresh_workspace(
    protocol_state: &mut ExtWorkspaceManagerState,
    workspace_index: usize,
    active: bool,
    output: Option<&Output>,
) -> bool {
    let workspace_groups = &protocol_state.workspace_groups;
    let mut state = ext_workspace_handle_v1::State::empty();
    if active {
        state |= ext_workspace_handle_v1::State::Active;
    }

    match protocol_state.workspaces.entry(workspace_index) {
        Entry::Occupied(entry) => {
            let workspace = entry.into_mut();

            let mut state_changed = false;
            if workspace.state != state {
                workspace.state = state;
                state_changed = true;
            }

            let mut output_changed = false;
            if workspace.output.as_ref() != output {
                send_workspace_enter_leave(workspace_groups, workspace, false);
                workspace.output = output.cloned();
                output_changed = true;
            }

            if output_changed {
                send_workspace_enter_leave(workspace_groups, workspace, true);
            }

            if state_changed {
                for handle in &workspace.instances {
                    handle.state(workspace.state);
                }
            }

            output_changed || state_changed
        }
        Entry::Vacant(entry) => {
            let mut workspace = WorkspaceData {
                id: build_workspace_name(workspace_index),
                name: build_workspace_name(workspace_index),
                coordinates: [0, workspace_index as u32],
                state,
                output: output.cloned(),
                instances: Vec::new(),
            };

            for manager in protocol_state.instances.keys() {
                if let Some(client) = manager.client() {
                    workspace.add_instance::<Raven>(&protocol_state.display, &client, manager);
                }
            }

            send_workspace_enter_leave(workspace_groups, &workspace, true);
            entry.insert(workspace);
            true
        }
    }
}

fn send_workspace_enter_leave(
    workspace_groups: &HashMap<Output, WorkspaceGroupData>,
    workspace: &WorkspaceData,
    enter: bool,
) {
    let Some(output) = &workspace.output else {
        return;
    };
    let Some(group_data) = workspace_groups.get(output) else {
        return;
    };

    for group in &group_data.instances {
        let manager: &ExtWorkspaceManagerV1 = group.data().expect("missing group manager data");
        for handle in &workspace.instances {
            if handle.data() != Some(manager) {
                continue;
            }
            if enter {
                group.workspace_enter(handle);
            } else {
                group.workspace_leave(handle);
            }
        }
    }
}

fn remove_workspace_instances(
    workspace_groups: &HashMap<Output, WorkspaceGroupData>,
    workspace: &WorkspaceData,
) {
    send_workspace_enter_leave(workspace_groups, workspace, false);
    for handle in &workspace.instances {
        handle.removed();
    }
}

impl WorkspaceGroupData {
    fn add_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
        output: &Output,
    ) where
        D: Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1>,
        D: 'static,
    {
        let group = client
            .create_resource::<ExtWorkspaceGroupHandleV1, _, D>(
                handle,
                manager.version(),
                manager.clone(),
            )
            .expect("failed to create ext_workspace_group handle");

        manager.workspace_group(&group);
        group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());

        for wl_output in output.client_outputs(client) {
            group.output_enter(&wl_output);
        }

        self.instances.push(group);
    }
}

impl WorkspaceData {
    fn add_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
    ) where
        D: Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1>,
        D: 'static,
    {
        let workspace = client
            .create_resource::<ExtWorkspaceHandleV1, _, D>(
                handle,
                manager.version(),
                manager.clone(),
            )
            .expect("failed to create ext_workspace handle");

        manager.workspace(&workspace);
        workspace.id(self.id.clone());
        workspace.name(self.name.clone());
        workspace.coordinates(
            self.coordinates
                .iter()
                .flat_map(|value| value.to_ne_bytes())
                .collect(),
        );
        workspace.state(self.state);
        workspace.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);

        self.instances.push(workspace);
    }
}

impl ExtWorkspaceManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ExtWorkspaceManagerV1, ExtWorkspaceGlobalData>,
        D: Dispatch<ExtWorkspaceManagerV1, ()>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ExtWorkspaceGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ExtWorkspaceManagerV1, _>(VERSION, global_data);

        Self {
            display: display.clone(),
            instances: HashMap::new(),
            workspace_groups: HashMap::new(),
            workspaces: HashMap::new(),
        }
    }
}

impl<D> GlobalDispatch<ExtWorkspaceManagerV1, ExtWorkspaceGlobalData, D>
    for ExtWorkspaceManagerState
where
    D: GlobalDispatch<ExtWorkspaceManagerV1, ExtWorkspaceGlobalData>,
    D: Dispatch<ExtWorkspaceManagerV1, ()>,
    D: Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1>,
    D: Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1>,
    D: ExtWorkspaceHandler,
{
    fn bind(
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &ExtWorkspaceGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let protocol_state = state.ext_workspace_manager_state();

        for (output, group_data) in &mut protocol_state.workspace_groups {
            group_data.add_instance::<D>(handle, client, &manager, output);
        }

        for workspace_data in protocol_state.workspaces.values_mut() {
            workspace_data.add_instance::<D>(handle, client, &manager);
        }

        // Send workspace_enter for handles belonging to this manager.
        for (output, group_data) in &protocol_state.workspace_groups {
            for group in &group_data.instances {
                if group.data() != Some(&manager) {
                    continue;
                }

                for workspace_data in protocol_state.workspaces.values() {
                    if workspace_data.output.as_ref() != Some(output) {
                        continue;
                    }
                    for workspace in &workspace_data.instances {
                        if workspace.data() == Some(&manager) {
                            group.workspace_enter(workspace);
                        }
                    }
                }
            }
        }

        manager.done();
        protocol_state.instances.insert(manager, Vec::new());
    }

    fn can_view(client: Client, global_data: &ExtWorkspaceGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<ExtWorkspaceManagerV1, (), D> for ExtWorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceManagerV1, ()>,
    D: ExtWorkspaceHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: <ExtWorkspaceManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let protocol_state = state.ext_workspace_manager_state();
                let Some(actions) = protocol_state.instances.get_mut(resource) else {
                    return;
                };
                let actions = std::mem::take(actions);

                for action in actions {
                    match action {
                        Action::Activate(index) => state.activate_workspace(index),
                    }
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                resource.finished();

                let protocol_state = state.ext_workspace_manager_state();
                protocol_state
                    .instances
                    .retain(|instance, _| instance != resource);

                for workspace in protocol_state.workspaces.values_mut() {
                    workspace
                        .instances
                        .retain(|instance| instance.data() != Some(resource));
                }

                for group in protocol_state.workspace_groups.values_mut() {
                    group
                        .instances
                        .retain(|instance| instance.data() != Some(resource));
                }
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, resource: &ExtWorkspaceManagerV1, _data: &()) {
        let protocol_state = state.ext_workspace_manager_state();
        protocol_state
            .instances
            .retain(|instance, _| instance != resource);
    }
}

impl<D> Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1, D> for ExtWorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1>,
    D: ExtWorkspaceHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceHandleV1,
        request: <ExtWorkspaceHandleV1 as Resource>::Request,
        manager: &ExtWorkspaceManagerV1,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let protocol_state = state.ext_workspace_manager_state();
        let Some((workspace_index, _)) = protocol_state
            .workspaces
            .iter()
            .find(|(_, workspace_data)| workspace_data.instances.contains(resource))
        else {
            return;
        };

        match request {
            ext_workspace_handle_v1::Request::Activate => {
                if let Some(actions) = protocol_state.instances.get_mut(manager) {
                    actions.push(Action::Activate(*workspace_index));
                }
            }
            ext_workspace_handle_v1::Request::Deactivate => {}
            ext_workspace_handle_v1::Request::Assign { .. } => {}
            ext_workspace_handle_v1::Request::Remove => {}
            ext_workspace_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ExtWorkspaceHandleV1,
        _data: &ExtWorkspaceManagerV1,
    ) {
        let protocol_state = state.ext_workspace_manager_state();
        for workspace in protocol_state.workspaces.values_mut() {
            workspace.instances.retain(|instance| instance != resource);
        }
    }
}

impl<D> Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1, D> for ExtWorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1>,
    D: ExtWorkspaceHandler,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: <ExtWorkspaceGroupHandleV1 as Resource>::Request,
        _data: &ExtWorkspaceManagerV1,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ExtWorkspaceGroupHandleV1,
        _data: &ExtWorkspaceManagerV1,
    ) {
        let protocol_state = state.ext_workspace_manager_state();
        for group in protocol_state.workspace_groups.values_mut() {
            group.instances.retain(|instance| instance != resource);
        }
    }
}

#[macro_export]
macro_rules! delegate_ext_workspace {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1: $crate::protocols::ext_workspace::ExtWorkspaceGlobalData
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1: ()
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_handle_v1::ExtWorkspaceHandleV1: smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1: smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
    };
}

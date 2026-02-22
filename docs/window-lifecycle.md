# Window Lifecycle Contract

This document defines the expected toplevel lifecycle used by Raven state and handlers.

## State machine

1. `new_toplevel`
- Window is tracked in `unmapped_workspaces`.
- Root surface is marked in `unmapped_toplevel_ids`.
- Initial configure is queued.
- Window is not mapped into `Space`.

2. First real root-buffer commit (`root_is_mapped && root_has_buffer`)
- Window is promoted from `unmapped_workspaces` to `workspaces`.
- Root surface is cleared from `unmapped_toplevel_ids`.
- Window may be mapped into `Space` if its workspace is currently active.

3. Root unmap commit (`tracked_mapped && !root_is_mapped`)
- Window is unmapped from `Space`.
- Window is demoted from `workspaces` to `unmapped_workspaces`.
- Root surface is marked in `unmapped_toplevel_ids`.
- Initial configure/rule recheck are queued for the next map.

4. `toplevel_destroyed`
- Remove window from both workspace stores.
- Clear pending/unmapped bookkeeping sets for the root surface.

## Fullscreen policy

- Fullscreen ownership is per workspace:
  - Entering fullscreen clears only fullscreen windows in the same workspace.
  - Fullscreen windows on other workspaces are kept intact.
- During fullscreen transitions, pending state is treated as the source of truth over stale committed state.

## Debug-only invariants

`debug_assert_state_invariants()` checks the following:

- A surface cannot exist in both mapped and unmapped workspace stores.
- Mapped `Space` elements must exist in mapped workspace tracking.
- Mapped `Space` elements cannot still be marked in `unmapped_toplevel_ids`.
- Live `unmapped_toplevel_ids` entries must exist in unmapped workspace tracking.
- `fullscreen_windows` entries must be unique and present in workspace tracking.

These checks are compiled in debug builds only (`cfg(debug_assertions)`).

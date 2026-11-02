# Initial Requirements

Status: evolving requirements. An initial implementation now exists; consult the
README for implemented features and verification limits. This document also
contains future work and must not be read as a completion checklist.

## 1. Goal and terminology

Build a Rust TUI that makes monitor configuration accessible without manually
editing Hyprland configuration files.

- **Extend:** a monitor occupies an independent region of the desktop.
- **Mirror / duplicate / copy:** a monitor displays the output of another
  monitor. These labels refer to the same operation, not three different modes.
- **Disable:** a connected monitor is temporarily inactive.
- **Source:** the enabled monitor whose output is mirrored by another monitor.

Each output can be configured independently, subject to compositor and hardware
constraints. An extended desktop may also include mirrored outputs.

## 2. Discovery and live state

- Detect active and connected-but-disabled monitors through Hyprland.
- Show connector, reported make/model, enabled state, resolution, refresh rate,
  scale, transform, logical position, and mirror source when available.
- List supported modes as reported by the compositor; do not invent modes.
- Provide explicit refresh and monitor connection/disconnection updates.
- Distinguish the current compositor state from unsaved draft changes.
- Report missing Hyprland sessions, unavailable IPC, and unsupported capabilities
  with actionable errors.
- Keep display serial numbers out of logs and exported diagnostics by default.

### Physical display identification

- Provide an Identify action that briefly shows large numbers on active physical
  displays, matching the numbers in the TUI list and arrangement preview.
- Keep numbering stable throughout an editing session; do not use a compositor
  ID as a permanent display identity.
- Use a separate Rust Wayland layer-shell overlay helper. A terminal UI alone
  cannot draw outside its terminal window.
- Labels must not steal keyboard focus, intercept pointer input, reserve desktop
  space, or change display configuration. Dismiss automatically and on request.
- Handle scale, rotation, output removal, and helper termination cleanly.
- Disabled displays cannot show labels without being enabled; identify them in
  the list without automatically enabling them.
- Mirrored outputs may repeat their source's label. Do not promise distinct
  physical labels in mirror mode until compositor behavior has been tested;
  explain mirror groups in the TUI rather than breaking mirroring to identify.

## 3. Layout editor

- Display a labeled terminal diagram of the logical monitor arrangement.
- Select monitors using the keyboard and move them relative to one another.
- Support left, right, above, below, and edge/center alignment.
- Allow precise numeric positions and explain the desktop coordinate origin.
- Calculate logical dimensions using resolution, scale, and rotation rather
  than raw pixel dimensions alone.
- Allow resolution, refresh rate, scale, and rotation changes per output.
- Filter refresh-rate choices by selected resolution and supported mode pairs.
- Provide normal, left, inverted, and right orientation options; expose reflected
  transforms separately if supported.
- Expose additional monitor settings through a capability-aware advanced view
  (candidates: VRR, bit depth, and color-management/HDR settings). Determine API
  support, validation, and rollback for each before enabling it. Do not claim
  arbitrary hardware brightness or every compositor option is universally
  configurable.
- This editor moves monitor regions, not Hyprland virtual workspaces. Workspace
  reassignment is outside the initial scope.
- Enable or disable individual monitors; reject disabling all outputs in the
  initial release.
- Clearly distinguish independent outputs from mirrored outputs in the diagram.
- Validate unintended overlap and gaps, with an explicit decision before apply.
- Keep editing responsive when terminal size changes; show a usable compact
  layout or minimum-size message when the diagram does not fit.

## 4. Mirroring

- Select a mirror source from enabled independent outputs.
- Reject self-mirroring, cycles, and mirror chains in the initial release.
- Validate source availability and compositor limitations before application.
- Explain differences in aspect ratio, scale, or resolution. Do not promise
  pixel-identical rendering across incompatible displays.
- Moving a mirrored output does not create additional independent desktop space.
- Returning an output to extended mode requires an explicit independent position.

## 5. Apply, confirmation, and recovery

- Edits remain a draft until the user requests Apply.
- Show the proposed changes before applying them.
- Capture the current runtime layout immediately before applying a draft.
- Require confirmation within a visible timeout; 15 seconds is the proposed
  default, subject to usability testing.
- Restore the captured layout on cancellation or confirmation timeout.
- Use a recovery watchdog independent of the TUI process so a UI crash does
  not cancel the rollback timer. Define its behavior on compositor exit.
- Check command results and re-query actual state: a successful command alone
  does not prove that the requested layout was achieved.
- Treat multi-output updates as potentially non-atomic. Define an application
  order that keeps an output usable, and attempt rollback after partial failure.
- If an output disappears, revalidate the draft and restore only feasible
  settings; retain a usable connected output when possible.
- Detect external layout changes and ask the user to refresh/reconcile before
  overwriting them.
- Runtime changes must not require root privileges.

## 6. Persistence and profiles

- Separate temporary runtime application from explicit Save.
- Define how saved configuration is loaded before presenting Save as persistent.
- Support current Hyprland configuration conventions, including Lua; investigate
  the supported API before choosing a generation format.
- Prefer an app-owned configuration fragment with documented integration over
  rewriting a user's entire compositor configuration.
- Preserve unrelated settings and create a backup before replacing managed
  persistent configuration.
- Offer named layouts such as laptop-only, desk, and presentation.
- Define matching rules for reconnecting displays; distinguish identical models
  and changed connector names without silently applying an ambiguous profile.
- Automatic profile switching is a later option, not initial default behavior.

## 7. TUI interaction

- Provide monitor list, arrangement preview, selected-output properties, and an
  always-discoverable help area.
- Support keyboard-only navigation and searchable/action-menu operations.
- Use arrows for navigation and a separate explicit movement mode to avoid
  accidental layout edits; finalize bindings during UI prototyping.
- In movement mode, arrow keys move the selected monitor in logical desktop
  coordinates. Provide fine/coarse steps and relative placement actions against
  a selected reference monitor, with edge and center snapping.
- In the properties panel, arrows navigate fields and choices; Enter opens or
  accepts an editor and Escape cancels. Resolution, frequency, orientation,
  scale, and output mode must all be editable without a mouse.
- Provide Apply, Confirm, Revert, Refresh, and Quit actions.
- Prompt about uncommitted draft changes when quitting.
- Use text/status symbols as well as color; allow integration with a future
  global desktop palette.
- Where terminal mouse reporting is available, support clicking monitor boxes
  and dragging them inside the TUI layout preview, with the same snapping and
  validation as keyboard edits. This edits the draft, never applies immediately.
- Mouse dragging is optional: keyboard controls remain complete when a terminal
  or multiplexer does not deliver mouse events. Restore terminal mouse reporting
  on exit and cancel a drag safely on resize or output removal.

## 8. Proposed architecture

- Pure layout model and validation independent of terminal rendering.
- Backend interface for discovery, supported capabilities, apply, and readback.
- Initial Hyprland backend using structured data and supported runtime APIs.
- Invoke commands with argument arrays, never interpolated shell command text.
- Separate recovery watchdog and persistence/profile storage.
- Separate graphical identification from terminal rendering; use Wayland client
  bindings and layer-shell support for an optional Rust overlay helper.
- Future control-panel integration can launch the standalone TUI; avoid coupling
  the first release to a control panel that does not yet exist.

## 9. Acceptance criteria

- Correctly discover a laptop display and an external display, including a
  connected output disabled in the compositor.
- Extend a desktop horizontally and vertically with correct logical geometry.
- Change supported display modes, scale, and rotation and verify readback.
- Identify active outputs with matching overlay/TUI numbers without losing TUI
  focus; test timeout, hotplug, mixed scaling, and mirror-group behavior.
- Complete selection, relative positioning, and property editing with only the
  keyboard; verify fine/coarse steps and snapping.
- Test optional drag selection, movement, cancellation, and resize handling in
  a supported terminal, plus keyboard operation with mouse reporting unavailable.
- Mirror an external display from the laptop and return it to extended mode.
- Verify mixed-scale and rotated layouts and a three-output layout where
  hardware is available; record untested combinations explicitly.
- Revert after timeout, cancellation, partial apply failure, and TUI termination.
- Handle disconnect/reconnect and external configuration changes without
  applying stale drafts silently.
- Verify saved layout loading in a fresh session before declaring persistence
  complete.
- Test layout validation and recovery with a fake backend before live changes.
- Run Cargo formatting, Clippy, and meaningful unit/integration tests once code
  exists. Live monitor changes require an explicitly approved test session.

## 10. Proposed milestones

1. Read-only Rust TUI: discovery, monitor list, and logical layout preview.
2. Draft layout editor: positioning, modes, scale, rotation, and validation.
   Include keyboard movement and properties; prototype optional mouse dragging
   and the independent display-identification overlay.
3. Runtime apply with independent timed rollback and state readback.
4. Mirroring, disabling, and hotplug recovery.
5. Explicit persistence, named profiles, and control-panel integration.

## 11. Decisions still open

- Final application name and default keybindings.
- Hyprland minimum version and runtime/configuration API compatibility.
- Profile storage format and persistence integration.
- Mirror behavior for mismatched display modes, based on backend capabilities.
- Recovery watchdog lifecycle and timeout configuration.
- Packaging and distribution license.

## 12. Feasibility assessment

The requested interaction is technically feasible, with a separate graphical
helper for physical display labels and terminal-dependent optional dragging.

- Read-only host inspection confirmed that `hyprctl -j monitors all` exposes
  geometry, scale, transform, mirror state, and supported modes. Only one active
  output was present during this assessment; multi-output behavior is untested.
- Crossterm exposes mouse press, release, and drag events. The application must
  implement hit testing and convert terminal-cell movement into logical display
  coordinates; precise numeric/keyboard editing remains necessary.
- The layer-shell protocol supports surfaces assigned to a specific `wl_output`,
  an overlay layer, no keyboard focus, and an empty pointer input region. It is
  supported by Hyprland; actual label rendering and mirrored-output behavior
  still require a prototype.
- Position and property editing are ordinary draft-model operations. Runtime
  application must use the APIs supported by the installed Hyprland version;
  successful discovery does not establish apply or rollback correctness.
- The implementation now includes a graphical helper and keyboard/mouse editing.
  A host smoke test observed the overlay layer appear and disappear, with
  unchanged focus and monitor settings. Unit and process tests cover simulated
  layouts and recovery. Multi-output hardware validation remains pending.

References:

- [Crossterm mouse events](https://docs.rs/crossterm/latest/crossterm/event/enum.MouseEventKind.html)
- [Wayland layer-shell protocol](https://wayland.app/protocols/wlr-layer-shell-unstable-v1)

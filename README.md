# hypr-monitor-tui

A planned Rust terminal user interface for configuring monitors on Hyprland.

Discover connected displays, arrange an extended desktop, mirror one display
onto another, and adjust display modes through a keyboard-driven interface.

## Project status

Requirements and design stage. No executable is implemented yet.

See [the initial requirements](docs/requirements.md) for scope, interaction,
safety constraints, and the proposed implementation milestones.

## Planned stack

- Rust.
- Ratatui and Crossterm for the terminal interface (proposed).
- Serde for structured Hyprland monitor data (proposed).
- A Hyprland-specific backend, isolated from the UI and arrangement model.

The first release targets Linux with an active Hyprland session. Other Wayland
compositors and X11 are outside its initial scope.

## Core workflow

1. Detect connected monitors and their supported modes.
2. Edit a draft layout without immediately changing the desktop.
3. Preview and validate the layout.
4. Apply temporarily and confirm that the result is usable.
5. Revert automatically if confirmation does not arrive in time.
6. Save explicitly for future sessions when persistence is supported.

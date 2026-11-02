//! Terminal palette roles for the TUI and pixel colors for Wayland overlays.
use ratatui::style::Color;

pub const ACCENT: Color = Color::Cyan;
pub const BORDER: Color = Color::Blue;
pub const TEXT: Color = Color::Reset;
pub const OVERLAY_FONT: &std::ffi::CStr = c"UbuntuMono Nerd Font";
pub const OVERLAY_BACKGROUND: u32 = 0xcc000000;
pub const OVERLAY_TEXT: u32 = 0xfff1ffff;

/// Read the compositor's active window border instead of duplicating its theme.
pub fn overlay_border() -> anyhow::Result<(u32, usize)> {
    use anyhow::{Context, ensure};
    let option = |name: &str| -> anyhow::Result<serde_json::Value> {
        let output = std::process::Command::new("hyprctl")
            .args(["-j", "getoption", name])
            .output()?;
        ensure!(
            output.status.success(),
            "Cannot read Hyprland option {name}"
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let color = option("general:col.active_border")?;
    let color = color["gradient"]
        .as_str()
        .and_then(|s| s.split_whitespace().next())
        .context("Hyprland did not return an active border color")?;
    let argb = u32::from_str_radix(color, 16)?;
    // Wayland and Cairo require premultiplied color channels.
    let alpha = argb >> 24;
    let channel = |shift: u32| (((argb >> shift) & 255u32) * alpha / 255) << shift;
    let color = (alpha << 24) | channel(16) | channel(8) | channel(0);
    let width = option("general:border_size")?["int"]
        .as_u64()
        .context("Hyprland did not return a border size")?;
    Ok((color, width.min(79) as usize))
}

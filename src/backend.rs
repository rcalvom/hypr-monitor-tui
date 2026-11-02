use crate::model::{Layout, Mode, Monitor, same_state};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn command(program: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Cannot start {program}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let stdout = thread::spawn(move || {
        let mut s = String::new();
        out.read_to_string(&mut s).map(|_| s)
    });
    let stderr = thread::spawn(move || {
        let mut s = String::new();
        err.read_to_string(&mut s).map(|_| s)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(4) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let out = stdout
        .join()
        .map_err(|_| anyhow::anyhow!("Output reader failed"))??;
    let err = stderr
        .join()
        .map_err(|_| anyhow::anyhow!("Error reader failed"))??;
    ensure!(status.success(), "{program}: {}", err.trim());
    Ok(out)
}

pub fn query() -> Result<Layout> {
    parse(&command("hyprctl", &["-j", "monitors", "all"])?)
}

pub fn parse(input: &str) -> Result<Layout> {
    let rows: Vec<Value> = serde_json::from_str(input).context("Invalid monitor JSON")?;
    let mut monitors = Vec::new();
    for row in &rows {
        let name = row["name"]
            .as_str()
            .context("Missing output name")?
            .to_string();
        let enabled = !row["disabled"].as_bool().unwrap_or(false);
        let mut modes: Vec<_> = row["availableModes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| Mode::parse(s.as_str()?))
            .collect();
        let mut mode = Mode {
            width: row["width"].as_i64().unwrap_or(0) as i32,
            height: row["height"].as_i64().unwrap_or(0) as i32,
            hz: row["refreshRate"].as_f64().unwrap_or(0.),
        };
        if mode.width <= 0 || mode.height <= 0 || mode.hz <= 0. {
            mode = modes
                .first()
                .cloned()
                .context("Output has no usable reported mode")?;
        }
        if enabled && !modes.iter().any(|m| m.matches(&mode)) {
            modes.push(mode.clone());
        }
        let mirror = match &row["mirrorOf"] {
            Value::String(s) if s != "none" && !s.is_empty() => Some(s.clone()),
            Value::Number(n) if n.as_i64().unwrap_or(-1) >= 0 => Some(n.to_string()),
            _ => None,
        }
        .map(|s| {
            rows.iter()
                .find(|r| r["id"].as_i64().map(|id| id.to_string()) == Some(s.clone()))
                .and_then(|r| r["name"].as_str())
                .unwrap_or(&s)
                .to_string()
        });
        monitors.push(Monitor {
            name,
            model: row["model"].as_str().unwrap_or("").into(),
            enabled,
            mode,
            modes,
            x: row["x"].as_i64().unwrap_or(0) as i32,
            y: row["y"].as_i64().unwrap_or(0) as i32,
            scale: row["scale"].as_f64().filter(|s| *s > 0.).unwrap_or(1.),
            transform: row["transform"].as_u64().unwrap_or(0) as u32,
            mirror,
        });
    }
    monitors.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(monitors)
}

// Lua byte escapes avoid both code injection and JSON/Lua Unicode escape mismatch.
fn quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.bytes().map(|b| format!("\\{b:03}")).collect::<String>()
    )
}

pub fn lua(m: &Monitor) -> String {
    if !m.enabled {
        return format!("hl.monitor({{output={},disabled=true}})", quote(&m.name));
    }
    format!(
        "hl.monitor({{output={},disabled=false,mode={},position={},scale={},transform={},mirror={}}})",
        quote(&m.name),
        quote(&m.mode.label()),
        quote(&format!("{}x{}", m.x, m.y)),
        m.scale,
        m.transform,
        quote(m.mirror.as_deref().unwrap_or(""))
    )
}

pub fn apply(layout: &[Monitor]) -> Result<()> {
    // Enable independent sources before mirrors, disable obsolete outputs last.
    for m in layout
        .iter()
        .filter(|m| m.independent())
        .chain(layout.iter().filter(|m| m.enabled && m.mirror.is_some()))
        .chain(layout.iter().filter(|m| !m.enabled))
    {
        let out = command("hyprctl", &["eval", &lua(m)])?;
        ensure!(
            out.trim() == "ok",
            "Hyprland rejected {}: {}",
            m.name,
            out.trim()
        );
    }
    verify(layout)
}

pub fn verify(layout: &[Monitor]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if same_state(layout, &query()?) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Compositor state differs from requested layout"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn recovery_layout(before: &[Monitor], current: &[Monitor]) -> Layout {
    let mut restored: Layout = current
        .iter()
        .map(|now| {
            before
                .iter()
                .find(|old| old.name == now.name)
                .cloned()
                .unwrap_or_else(|| now.clone())
        })
        .collect();
    let sources: Vec<_> = restored
        .iter()
        .filter(|m| m.independent())
        .map(|m| m.name.clone())
        .collect();
    for m in &mut restored {
        if m.mirror.as_ref().is_some_and(|s| !sources.contains(s)) {
            m.mirror = None;
        }
    }
    if !restored.iter().any(Monitor::independent)
        && let Some(m) = restored.first_mut()
    {
        m.enabled = true;
        m.mirror = None;
        m.x = 0;
        m.y = 0;
    }
    restored
}

pub fn restore(before: &[Monitor]) -> Result<()> {
    let current = query()?;
    ensure!(
        !current.is_empty(),
        "No connected output available for recovery"
    );
    let restored = recovery_layout(before, &current);
    if let Err(error) = apply(&restored) {
        // Emergency fallback uses currently reported modes rather than stale ones.
        let mut fallback = query()?;
        ensure!(
            !fallback.is_empty(),
            "Rollback failed ({error}); no outputs remain"
        );
        for (i, m) in fallback.iter_mut().enumerate() {
            m.enabled = i == 0;
            m.mirror = None;
            m.x = 0;
            m.y = 0;
            m.scale = 1.;
            m.transform = 0;
            if let Some(mode) = m.modes.first() {
                m.mode = mode.clone();
            }
        }
        apply(&fallback).context("Rollback and emergency recovery both failed")?;
        bail!("Original layout unavailable ({error}); enabled a fallback output instead");
    }
    Ok(())
}

pub fn check_version() -> Result<()> {
    let out = command("hyprctl", &["version"])?;
    ensure!(
        out.contains("Hyprland 0.57."),
        "Runtime apply currently supports Hyprland 0.57.x (Lua API). Read-only discovery remains available."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_numeric_mirror_and_no_serials() {
        let data = r#"[{"id":0,"name":"A","width":1920,"height":1080,"refreshRate":60,"serial":"private","availableModes":["1920x1080@60Hz"]},{"id":1,"name":"B","width":1920,"height":1080,"refreshRate":60,"mirrorOf":0,"availableModes":["1920x1080@60Hz"]}]"#;
        let l = parse(data).unwrap();
        assert_eq!(l[1].mirror.as_deref(), Some("A"));
        assert!(!serde_json::to_string(&l).unwrap().contains("private"));
        let recover = recovery_layout(&l, &l[1..]);
        assert!(recover[0].independent());
    }
    #[test]
    fn quote_is_always_numeric_bytes() {
        assert_eq!(quote("\"\né"), "\"\\034\\010\\195\\169\"");
    }
}

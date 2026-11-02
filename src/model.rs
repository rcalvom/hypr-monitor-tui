use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    pub hz: f64,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        let (size, hz) = value.trim_end_matches("Hz").split_once('@')?;
        let (w, h) = size.split_once('x')?;
        let mode = Self {
            width: w.parse().ok()?,
            height: h.parse().ok()?,
            hz: hz.parse().ok()?,
        };
        (mode.width > 0 && mode.height > 0 && mode.hz.is_finite() && mode.hz > 0.0).then_some(mode)
    }

    pub fn label(&self) -> String {
        format!("{}x{}@{:.3}Hz", self.width, self.height, self.hz)
    }

    pub fn matches(&self, other: &Self) -> bool {
        self.width == other.width && self.height == other.height && (self.hz - other.hz).abs() < 0.2
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub model: String,
    pub enabled: bool,
    pub mode: Mode,
    pub modes: Vec<Mode>,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub mirror: Option<String>,
}

pub type Layout = Vec<Monitor>;

impl Monitor {
    pub fn size(&self) -> (i32, i32) {
        let (w, h) = if self.transform % 2 == 1 {
            (self.mode.height, self.mode.width)
        } else {
            (self.mode.width, self.mode.height)
        };
        (
            (w as f64 / self.scale).round() as i32,
            (h as f64 / self.scale).round() as i32,
        )
    }
    pub fn independent(&self) -> bool {
        self.enabled && self.mirror.is_none()
    }
}

pub fn validate(layout: &[Monitor]) -> Result<Vec<String>> {
    ensure!(!layout.is_empty(), "No connected monitors");
    ensure!(
        layout.iter().any(Monitor::independent),
        "Keep at least one independent output enabled"
    );
    let mut names = std::collections::HashSet::new();
    let mut warnings = Vec::new();
    for m in layout {
        ensure!(
            !m.name.is_empty() && names.insert(&m.name),
            "Duplicate or empty connector"
        );
        ensure!(
            m.x.abs_diff(0) <= 100_000 && m.y.abs_diff(0) <= 100_000,
            "Position outside supported range"
        );
        if !m.enabled {
            continue;
        }
        ensure!(
            (1..=32768).contains(&m.mode.width)
                && (1..=32768).contains(&m.mode.height)
                && m.mode.hz.is_finite()
                && m.mode.hz > 0.0
                && m.mode.hz <= 2000.0,
            "{}: invalid display mode",
            m.name
        );
        ensure!(
            m.scale.is_finite() && (0.25..=4.0).contains(&m.scale),
            "{}: scale must be 0.25–4",
            m.name
        );
        ensure!(m.transform <= 7, "Invalid transform");
        ensure!(
            m.modes.iter().any(|mode| mode.matches(&m.mode)),
            "{}: unsupported resolution/refresh pair",
            m.name
        );
        let (w, h) = m.size();
        ensure!(
            w > 0 && h > 0 && w <= 32768 && h <= 32768,
            "Invalid logical size"
        );
        // Hyprland requires an integral logical size for a valid scale.
        ensure!(
            (m.mode.width as f64 / m.scale).fract().abs() < 0.01
                && (m.mode.height as f64 / m.scale).fract().abs() < 0.01,
            "{}: scale produces fractional logical dimensions",
            m.name
        );
        if let Some(source) = &m.mirror {
            ensure!(source != &m.name, "Cannot mirror itself");
            let source = layout
                .iter()
                .find(|s| &s.name == source)
                .ok_or_else(|| anyhow::anyhow!("Mirror source missing"))?;
            ensure!(
                source.independent(),
                "Mirror source must be enabled and independent (no chains)"
            );
            if i64::from(source.mode.width) * i64::from(m.mode.height)
                != i64::from(m.mode.width) * i64::from(source.mode.height)
            {
                warnings.push(format!(
                    "{}: mirror aspect ratio differs from {}",
                    m.name, source.name
                ));
            }
        }
    }
    let independent: Vec<_> = layout.iter().filter(|m| m.independent()).collect();
    for (i, a) in independent.iter().enumerate() {
        let (aw, ah) = a.size();
        for b in &independent[i + 1..] {
            let (bw, bh) = b.size();
            if a.x < b.x + bw && b.x < a.x + aw && a.y < b.y + bh && b.y < a.y + ah {
                warnings.push(format!("{} overlaps {}", a.name, b.name));
            }
        }
    }
    // Detect islands, not merely a gap between arbitrary non-neighboring outputs.
    let mut connected = vec![false; independent.len()];
    connected[0] = true;
    loop {
        let before = connected.clone();
        for (i, a) in independent.iter().enumerate() {
            if !before[i] {
                continue;
            }
            let (aw, ah) = a.size();
            for (j, b) in independent.iter().enumerate() {
                let (bw, bh) = b.size();
                let x_overlap = a.x < b.x + bw && b.x < a.x + aw;
                let y_overlap = a.y < b.y + bh && b.y < a.y + ah;
                if (x_overlap && a.y <= b.y + bh && b.y <= a.y + ah)
                    || (y_overlap && a.x <= b.x + bw && b.x <= a.x + aw)
                {
                    connected[j] = true;
                }
            }
        }
        if before == connected {
            break;
        }
    }
    if connected.contains(&false) {
        warnings.push("Layout contains disconnected regions (gaps)".into());
    }
    Ok(warnings)
}

pub fn same_state(a: &[Monitor], b: &[Monitor]) -> bool {
    a.len() == b.len()
        && a.iter().all(|m| {
            b.iter().find(|n| n.name == m.name).is_some_and(|n| {
                m.enabled == n.enabled
                    && (!m.enabled
                        || (m.mode.matches(&n.mode)
                            && (m.scale - n.scale).abs() < 0.01
                            && m.transform == n.transform
                            && m.mirror == n.mirror
                            && (m.mirror.is_some() || (m.x == n.x && m.y == n.y))))
            })
        })
}

pub fn place(
    layout: &mut [Monitor],
    selected: usize,
    reference: usize,
    direction: char,
) -> Result<()> {
    ensure!(selected != reference, "Select another reference monitor");
    ensure!(
        layout[selected].independent() && layout[reference].independent(),
        "Positioning requires extended outputs"
    );
    let a = &layout[reference];
    let (aw, ah) = a.size();
    let (bw, bh) = layout[selected].size();
    let (x, y) = match direction {
        'h' => (a.x - bw, a.y),
        'l' => (a.x + aw, a.y),
        'k' => (a.x, a.y - bh),
        'j' => (a.x, a.y + ah),
        'c' => (layout[selected].x, a.y + (ah - bh) / 2),
        'v' => (a.x + (aw - bw) / 2, layout[selected].y),
        _ => bail!("Unknown placement"),
    };
    layout[selected].x = x;
    layout[selected].y = y;
    Ok(())
}

pub fn snap(layout: &mut [Monitor], selected: usize, threshold: i32) {
    let m = &layout[selected];
    let (w, h) = m.size();
    let mut dx = threshold + 1;
    let mut dy = threshold + 1;
    for (i, other) in layout.iter().enumerate() {
        if i == selected || !other.independent() {
            continue;
        }
        let (ow, oh) = other.size();
        for x in [
            other.x - w,
            other.x,
            other.x + ow,
            other.x + ow - w,
            other.x + (ow - w) / 2,
        ] {
            if (x - m.x).abs() < dx.abs() {
                dx = x - m.x;
            }
        }
        for y in [
            other.y - h,
            other.y,
            other.y + oh,
            other.y + oh - h,
            other.y + (oh - h) / 2,
        ] {
            if (y - m.y).abs() < dy.abs() {
                dy = y - m.y;
            }
        }
    }
    if dx.abs() <= threshold {
        layout[selected].x += dx;
    }
    if dy.abs() <= threshold {
        layout[selected].y += dy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn monitor(name: &str, x: i32) -> Monitor {
        let mode = Mode::parse("1920x1080@60Hz").unwrap();
        Monitor {
            name: name.into(),
            model: String::new(),
            enabled: true,
            mode: mode.clone(),
            modes: vec![mode],
            x,
            y: 0,
            scale: 1.0,
            transform: 0,
            mirror: None,
        }
    }
    #[test]
    fn rotated_scaled_placement() {
        let mut a = vec![monitor("A", 0), monitor("B", 0)];
        a[0].scale = 1.5;
        a[0].transform = 1;
        place(&mut a, 1, 0, 'l').unwrap();
        assert_eq!((a[1].x, a[1].y), (720, 0));
        assert!(validate(&a).unwrap().is_empty());
    }
    #[test]
    fn disallow_cycles_and_all_disabled() {
        let mut a = vec![monitor("A", 0), monitor("B", 1920)];
        a[0].mirror = Some("B".into());
        a[1].mirror = Some("A".into());
        assert!(validate(&a).is_err());
        for m in &mut a {
            m.enabled = false;
        }
        assert!(validate(&a).is_err());
    }
    #[test]
    fn unsupported_pairs_and_fractional_scale() {
        let mut a = vec![monitor("A", 0)];
        a[0].mode.hz = 144.;
        assert!(validate(&a).is_err());
        a[0].mode.hz = 60.;
        a[0].scale = 1.7;
        assert!(validate(&a).is_err());
    }
    #[test]
    fn gaps_overlap_and_snap() {
        let mut a = vec![monitor("A", 0), monitor("B", 1930)];
        assert!(!validate(&a).unwrap().is_empty());
        snap(&mut a, 1, 20);
        assert_eq!(a[1].x, 1920);
        assert!(validate(&a).unwrap().is_empty());
        a[1].x = 1900;
        assert!(validate(&a).unwrap()[0].contains("overlaps"));
    }
    #[test]
    fn mode_parser_rejects_nonfinite() {
        assert!(Mode::parse("1920x1080@NaNHz").is_none());
    }
}

//! Multi-monitor and mixed-HiDPI virtual desktop geometry helpers.
//!
//! Coordinates are always in the OS's physical/native virtual desktop pixel space
//! (Per-Monitor DPI Aware V2 on Windows, RandR CRTC space on Linux X11).

use glide_proto::Edge;

/// Physical monitor rectangle in virtual desktop coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorRect {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
    pub primary: bool,
}

impl MonitorRect {
    pub const fn left(&self) -> i32 {
        self.x
    }

    pub const fn right(&self) -> i32 {
        self.x + self.width
    }

    pub const fn top(&self) -> i32 {
        self.y
    }

    pub const fn bottom(&self) -> i32 {
        self.y + self.height
    }

    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left() && x < self.right() && y >= self.top() && y < self.bottom()
    }
}

/// Complete multi-monitor layout of a machine.
#[derive(Debug, Clone, Default)]
pub struct DesktopGeometry {
    pub monitors: Vec<MonitorRect>,
}

impl DesktopGeometry {
    pub fn new(monitors: Vec<MonitorRect>) -> Self {
        Self { monitors }
    }

    /// Fallback 1920x1080 single-monitor geometry if OS query returns empty.
    pub fn fallback() -> Self {
        Self {
            monitors: vec![MonitorRect {
                name: "default".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: true,
            }],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// Returns the primary monitor (or the first monitor if none marked primary).
    pub fn primary(&self) -> Option<&MonitorRect> {
        self.monitors
            .iter()
            .find(|m| m.primary)
            .or_else(|| self.monitors.first())
    }

    /// Returns the center `(x, y)` of the primary monitor (for parking the cursor in remote mode).
    pub fn primary_center(&self) -> (i32, i32) {
        if let Some(m) = self.primary() {
            (m.x + m.width / 2, m.y + m.height / 2)
        } else {
            (960, 540)
        }
    }

    /// Returns the extreme bounding box `(min_x, min_y, max_x, max_y)` across all monitors.
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        if self.monitors.is_empty() {
            return (0, 0, 1920, 1080);
        }
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for m in &self.monitors {
            min_x = min_x.min(m.left());
            min_y = min_y.min(m.top());
            max_x = max_x.max(m.right());
            max_y = max_y.max(m.bottom());
        }
        (min_x, min_y, max_x, max_y)
    }

    /// Returns all monitors whose boundary lies on the specified outer edge of the desktop.
    fn monitors_on_edge(&self, edge: Edge) -> Vec<&MonitorRect> {
        let (min_x, min_y, max_x, max_y) = self.bounds();
        const TOL: i32 = 4;
        self.monitors
            .iter()
            .filter(|m| match edge {
                Edge::Left => (m.left() - min_x).abs() <= TOL,
                Edge::Right => (m.right() - max_x).abs() <= TOL,
                Edge::Top => (m.top() - min_y).abs() <= TOL,
                Edge::Bottom => (m.bottom() - max_y).abs() <= TOL,
            })
            .collect()
    }

    /// Checks whether cursor coordinate `(x, y)` is touching/beyond the given outer `edge`
    /// within `margin_px` pixels, returning the normalized position `t ∈ [0.0, 1.0]` along
    /// that edge's real monitor span.
    pub fn hit_outer_edge(&self, edge: Edge, x: i32, y: i32, margin_px: i32) -> Option<f32> {
        let edge_mons = self.monitors_on_edge(edge);
        if edge_mons.is_empty() {
            return None;
        }
        let margin = margin_px.max(0);
        let touching = edge_mons.iter().any(|m| match edge {
            Edge::Left => x <= m.left() + margin && y >= m.top() && y <= m.bottom(),
            Edge::Right => x >= m.right() - 1 - margin && y >= m.top() && y <= m.bottom(),
            Edge::Top => y <= m.top() + margin && x >= m.left() && x <= m.right(),
            Edge::Bottom => y >= m.bottom() - 1 - margin && x >= m.left() && x <= m.right(),
        });
        if !touching {
            return None;
        }
        Some(self.normalize_along_edge(edge, x, y, &edge_mons))
    }

    /// Distance in pixels from `(x, y)` to the outer `edge` (0 when on/past the edge).
    pub fn distance_to_edge(&self, edge: Edge, x: i32, y: i32) -> i32 {
        let (min_x, min_y, max_x, max_y) = self.bounds();
        match edge {
            Edge::Left => (x - min_x).max(0),
            Edge::Right => ((max_x - 1) - x).max(0),
            Edge::Top => (y - min_y).max(0),
            Edge::Bottom => ((max_y - 1) - y).max(0),
        }
    }

    fn normalize_along_edge(&self, edge: Edge, x: i32, y: i32, edge_mons: &[&MonitorRect]) -> f32 {
        match edge {
            Edge::Left | Edge::Right => {
                let min_y = edge_mons.iter().map(|m| m.top()).min().unwrap_or(0);
                let max_y = edge_mons.iter().map(|m| m.bottom()).max().unwrap_or(1080);
                let span = (max_y - min_y).max(1) as f32;
                ((y - min_y) as f32 / span).clamp(0.0, 1.0)
            }
            Edge::Top | Edge::Bottom => {
                let min_x = edge_mons.iter().map(|m| m.left()).min().unwrap_or(0);
                let max_x = edge_mons.iter().map(|m| m.right()).max().unwrap_or(1920);
                let span = (max_x - min_x).max(1) as f32;
                ((x - min_x) as f32 / span).clamp(0.0, 1.0)
            }
        }
    }

    /// Computes a safe cursor entry point `(x, y)` inside a real monitor touching `edge`,
    /// inset by `inset_px` pixels so it does not immediately bounce back across the boundary.
    pub fn point_on_edge(&self, edge: Edge, normalized_pos: f32, inset_px: i32) -> (i32, i32) {
        let t = normalized_pos.clamp(0.0, 1.0);
        let edge_mons = self.monitors_on_edge(edge);
        if edge_mons.is_empty() {
            return self.primary_center();
        }
        let inset = inset_px.max(1);

        match edge {
            Edge::Left | Edge::Right => {
                let min_y = edge_mons.iter().map(|m| m.top()).min().unwrap_or(0);
                let max_y = edge_mons.iter().map(|m| m.bottom()).max().unwrap_or(1080);
                let target_y = min_y + ((max_y - min_y - 1).max(0) as f32 * t).round() as i32;
                // Pick the monitor on this edge whose vertical range is closest to target_y
                let mon = edge_mons
                    .iter()
                    .min_by_key(|m| {
                        if target_y < m.top() {
                            m.top() - target_y
                        } else if target_y >= m.bottom() {
                            target_y - (m.bottom() - 1)
                        } else {
                            0
                        }
                    })
                    .unwrap();
                let clamped_y = target_y.clamp(mon.top() + 1, (mon.bottom() - 2).max(mon.top()));
                let x = match edge {
                    Edge::Left => (mon.left() + inset).min(mon.right() - 1),
                    Edge::Right => (mon.right() - 1 - inset).max(mon.left()),
                    _ => unreachable!(),
                };
                (x, clamped_y)
            }
            Edge::Top | Edge::Bottom => {
                let min_x = edge_mons.iter().map(|m| m.left()).min().unwrap_or(0);
                let max_x = edge_mons.iter().map(|m| m.right()).max().unwrap_or(1920);
                let target_x = min_x + ((max_x - min_x - 1).max(0) as f32 * t).round() as i32;
                let mon = edge_mons
                    .iter()
                    .min_by_key(|m| {
                        if target_x < m.left() {
                            m.left() - target_x
                        } else if target_x >= m.right() {
                            target_x - (m.right() - 1)
                        } else {
                            0
                        }
                    })
                    .unwrap();
                let clamped_x = target_x.clamp(mon.left() + 1, (mon.right() - 2).max(mon.left()));
                let y = match edge {
                    Edge::Top => (mon.top() + inset).min(mon.bottom() - 1),
                    Edge::Bottom => (mon.bottom() - 1 - inset).max(mon.top()),
                    _ => unreachable!(),
                };
                (clamped_x, y)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_dpi_multi_monitor_edge_mapping() {
        // Left 1080p @ 100%, Right 1440p @ 150% (physical pixels)
        let geom = DesktopGeometry::new(vec![
            MonitorRect {
                name: "DP-1".into(),
                x: -1920,
                y: 180,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: false,
            },
            MonitorRect {
                name: "DP-2".into(),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.5,
                primary: true,
            },
        ]);

        // Right edge is DP-2 (x=2559, y=0..1440)
        let hit = geom.hit_outer_edge(Edge::Right, 2559, 720, 0).unwrap();
        assert!((hit - 0.5).abs() < 0.01);

        // Left edge is DP-1 (x=-1920, y=180..1260) — NOT y=0..1440!
        let left_mid = geom.hit_outer_edge(Edge::Left, -1920, 720, 0).unwrap();
        assert!((left_mid - 0.5).abs() < 0.01);

        // Entering on left edge at 0.0 lands safely inside DP-1, never in the dead zone y=0..179
        let (ex, ey) = geom.point_on_edge(Edge::Left, 0.0, 4);
        assert_eq!(ex, -1916);
        assert!((180..1260).contains(&ey));
    }
}

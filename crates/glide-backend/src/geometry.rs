//! Multi-monitor and mixed-HiDPI virtual desktop geometry helpers.
//!
//! Coordinates are always in the OS's physical/native virtual desktop pixel space
//! (Per-Monitor DPI Aware V2 on Windows, RandR CRTC space on Linux X11).

use glide_proto::Edge;
use serde::{Deserialize, Serialize};

/// Physical monitor rectangle in virtual desktop coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

    /// Returns the primary monitor's DPI scale factor (e.g. `1.0`, `1.25`, `1.5`, `2.0`).
    pub fn primary_scale(&self) -> f32 {
        self.primary().map(|m| m.scale).unwrap_or(1.0).max(0.5)
    }

    /// Returns the center `(x, y)` of the primary monitor (for parking the cursor in remote mode).
    pub fn primary_center(&self) -> (i32, i32) {
        if let Some(m) = self.primary() {
            (m.x + m.width / 2, m.y + m.height / 2)
        } else {
            (960, 540)
        }
    }

    /// Returns the extreme bottom-right pixel `(x, y)` of the primary monitor (for invisible cursor parking on Windows).
    pub fn primary_park_corner(&self) -> (i32, i32) {
        if let Some(m) = self.primary() {
            ((m.right() - 1).max(m.left()), (m.bottom() - 1).max(m.top()))
        } else {
            (1919, 1079)
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

    /// Maps an arbitrary cursor `(x, y)` onto normalized `[0.0, 1.0] x [0.0, 1.0]` desktop coordinates.
    pub fn normalize_point(&self, x: i32, y: i32) -> (f32, f32) {
        let (min_x, min_y, max_x, max_y) = self.bounds();
        let w = (max_x - min_x).max(1) as f32;
        let h = (max_y - min_y).max(1) as f32;
        (
            ((x - min_x) as f32 / w).clamp(0.0, 1.0),
            ((y - min_y) as f32 / h).clamp(0.0, 1.0),
        )
    }

    /// Maps normalized `(nx, ny) ∈ [0.0, 1.0]^2` back to a safe pixel inside a real monitor.
    pub fn denormalize_point(&self, nx: f32, ny: f32) -> (i32, i32) {
        let (min_x, min_y, max_x, max_y) = self.bounds();
        let tx = min_x + ((max_x - min_x - 1).max(0) as f32 * nx.clamp(0.0, 1.0)).round() as i32;
        let ty = min_y + ((max_y - min_y - 1).max(0) as f32 * ny.clamp(0.0, 1.0)).round() as i32;
        if self.monitors.iter().any(|m| m.contains(tx, ty)) {
            return (tx, ty);
        }
        if let Some(mon) = self.monitors.iter().min_by_key(|m| {
            let cx = tx.clamp(m.left(), m.right() - 1);
            let cy = ty.clamp(m.top(), m.bottom() - 1);
            (tx - cx).abs() + (ty - cy).abs()
        }) {
            return (
                tx.clamp(mon.left() + 1, (mon.right() - 2).max(mon.left())),
                ty.clamp(mon.top() + 1, (mon.bottom() - 2).max(mon.top())),
            );
        }
        self.primary_center()
    }

    #[inline]
    fn monitor_on_edge(m: &MonitorRect, edge: Edge, bounds: (i32, i32, i32, i32)) -> bool {
        const TOL: i32 = 4;
        let (min_x, min_y, max_x, max_y) = bounds;
        match edge {
            Edge::Left => (m.left() - min_x).abs() <= TOL,
            Edge::Right => (m.right() - max_x).abs() <= TOL,
            Edge::Top => (m.top() - min_y).abs() <= TOL,
            Edge::Bottom => (m.bottom() - max_y).abs() <= TOL,
        }
    }

    /// Checks whether cursor coordinate `(x, y)` is touching/beyond the given outer `edge`
    /// within `margin_px` pixels (with no dead-corner margin).
    pub fn hit_outer_edge(&self, edge: Edge, x: i32, y: i32, margin_px: i32) -> Option<f32> {
        self.hit_outer_edge_guarded(edge, x, y, margin_px, 0)
    }

    /// Checks whether `(x, y)` touches outer `edge` while ignoring `dead_corner_px` pixels
    /// near each corner of the monitor (so slamming into Windows' top-right close `X` or
    /// bottom-right Show Desktop never accidentally switches screens).
    pub fn hit_outer_edge_guarded(
        &self,
        edge: Edge,
        x: i32,
        y: i32,
        margin_px: i32,
        dead_corner_px: i32,
    ) -> Option<f32> {
        if self.monitors.is_empty() {
            return None;
        }
        let bounds = self.bounds();
        let margin = margin_px.max(0);
        let dead = dead_corner_px.max(0);
        let mut touching = false;
        let mut span_min = i32::MAX;
        let mut span_max = i32::MIN;

        for m in &self.monitors {
            if !Self::monitor_on_edge(m, edge, bounds) {
                continue;
            }
            match edge {
                Edge::Left => {
                    span_min = span_min.min(m.top());
                    span_max = span_max.max(m.bottom());
                    let d = dead.min(m.height / 4);
                    if x <= m.left() + margin && y >= m.top() + d && y <= m.bottom() - d {
                        touching = true;
                    }
                }
                Edge::Right => {
                    span_min = span_min.min(m.top());
                    span_max = span_max.max(m.bottom());
                    let d = dead.min(m.height / 4);
                    if x >= m.right() - 1 - margin && y >= m.top() + d && y <= m.bottom() - d {
                        touching = true;
                    }
                }
                Edge::Top => {
                    span_min = span_min.min(m.left());
                    span_max = span_max.max(m.right());
                    let d = dead.min(m.width / 4);
                    if y <= m.top() + margin && x >= m.left() + d && x <= m.right() - d {
                        touching = true;
                    }
                }
                Edge::Bottom => {
                    span_min = span_min.min(m.left());
                    span_max = span_max.max(m.right());
                    let d = dead.min(m.width / 4);
                    if y >= m.bottom() - 1 - margin && x >= m.left() + d && x <= m.right() - d {
                        touching = true;
                    }
                }
            }
        }

        if !touching || span_min == i32::MAX {
            return None;
        }
        let span = (span_max - span_min).max(1) as f32;
        let coord = match edge {
            Edge::Left | Edge::Right => y,
            Edge::Top | Edge::Bottom => x,
        };
        Some(((coord - span_min) as f32 / span).clamp(0.0, 1.0))
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

    /// Computes a safe cursor entry point `(x, y)` inside a real monitor touching `edge`,
    /// inset by `inset_px` pixels so it does not immediately bounce back across the boundary.
    pub fn point_on_edge(&self, edge: Edge, normalized_pos: f32, inset_px: i32) -> (i32, i32) {
        if self.monitors.is_empty() {
            return self.primary_center();
        }
        let t = normalized_pos.clamp(0.0, 1.0);
        let bounds = self.bounds();
        let inset = inset_px.max(1);

        match edge {
            Edge::Left | Edge::Right => {
                let mut min_y = i32::MAX;
                let mut max_y = i32::MIN;
                for m in &self.monitors {
                    if Self::monitor_on_edge(m, edge, bounds) {
                        min_y = min_y.min(m.top());
                        max_y = max_y.max(m.bottom());
                    }
                }
                if min_y == i32::MAX {
                    return self.primary_center();
                }
                let target_y = min_y + ((max_y - min_y - 1).max(0) as f32 * t).round() as i32;
                // Pick the monitor on this edge whose vertical range is closest to target_y
                let Some(mon) = self
                    .monitors
                    .iter()
                    .filter(|m| Self::monitor_on_edge(m, edge, bounds))
                    .min_by_key(|m| {
                        if target_y < m.top() {
                            m.top() - target_y
                        } else if target_y >= m.bottom() {
                            target_y - (m.bottom() - 1)
                        } else {
                            0
                        }
                    })
                else {
                    return self.primary_center();
                };
                let clamped_y = target_y.clamp(mon.top() + 1, (mon.bottom() - 2).max(mon.top()));
                let x = match edge {
                    Edge::Left => (mon.left() + inset).min(mon.right() - 1),
                    Edge::Right => (mon.right() - 1 - inset).max(mon.left()),
                    _ => unreachable!(),
                };
                (x, clamped_y)
            }
            Edge::Top | Edge::Bottom => {
                let mut min_x = i32::MAX;
                let mut max_x = i32::MIN;
                for m in &self.monitors {
                    if Self::monitor_on_edge(m, edge, bounds) {
                        min_x = min_x.min(m.left());
                        max_x = max_x.max(m.right());
                    }
                }
                if min_x == i32::MAX {
                    return self.primary_center();
                }
                let target_x = min_x + ((max_x - min_x - 1).max(0) as f32 * t).round() as i32;
                let Some(mon) = self
                    .monitors
                    .iter()
                    .filter(|m| Self::monitor_on_edge(m, edge, bounds))
                    .min_by_key(|m| {
                        if target_x < m.left() {
                            m.left() - target_x
                        } else if target_x >= m.right() {
                            target_x - (m.right() - 1)
                        } else {
                            0
                        }
                    })
                else {
                    return self.primary_center();
                };
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

        // Dead corner guard: top-right X button (y=10) and bottom-right Show Desktop (y=1430) are blocked!
        assert!(geom
            .hit_outer_edge_guarded(Edge::Right, 2559, 10, 0, 24)
            .is_none());
        assert!(geom
            .hit_outer_edge_guarded(Edge::Right, 2559, 1430, 0, 24)
            .is_none());
        assert!(geom
            .hit_outer_edge_guarded(Edge::Right, 2559, 500, 0, 24)
            .is_some());

        // Normalized cursor point roundtrip
        let (nx, ny) = geom.normalize_point(1280, 720);
        let (rx, ry) = geom.denormalize_point(nx, ny);
        assert!((rx - 1280).abs() <= 2 && (ry - 720).abs() <= 2);
    }

    #[test]
    fn monitor_hotplug_and_dpi_change_recompute_edges_cleanly() {
        let single = DesktopGeometry::new(vec![MonitorRect {
            name: "eDP-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        }]);
        assert!(single.hit_outer_edge(Edge::Right, 1919, 540, 0).is_some());

        // Hotplug a 2560x1440 monitor on the right at 150% DPI
        let dual = DesktopGeometry::new(vec![
            MonitorRect {
                name: "eDP-1".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                primary: true,
            },
            MonitorRect {
                name: "DP-2".into(),
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.5,
                primary: false,
            },
        ]);
        assert_ne!(single, dual);
        // Old right border (x=1919) is now an internal seam — must NOT trigger outer Right edge!
        assert!(dual.hit_outer_edge(Edge::Right, 1919, 540, 0).is_none());
        // New right border (x=4479) triggers cleanly.
        assert!(dual.hit_outer_edge(Edge::Right, 4479, 720, 0).is_some());

        // Unplugging back to single monitor clamps previously valid right-monitor coords safely inside eDP-1
        let (rx, ry) = single.denormalize_point(0.95, 0.5);
        assert!((0..1920).contains(&rx) && (0..1080).contains(&ry));
        let (px, py) = single.primary_park_corner();
        assert_eq!((px, py), (1919, 1079));

        // DPI change on same resolution is detected by PartialEq and updates primary_scale()
        let scaled = DesktopGeometry::new(vec![MonitorRect {
            name: "eDP-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.5,
            primary: true,
        }]);
        assert_ne!(single, scaled);
        assert!((scaled.primary_scale() - 1.5).abs() < f32::EPSILON);
    }
}

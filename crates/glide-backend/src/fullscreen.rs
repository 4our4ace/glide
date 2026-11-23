//! Cross-platform fullscreen application classification and pointer-confinement helpers.
//!
//! Distinguishes fullscreen video players and non-pointer-locked fullscreen browsers
//! (e.g. YouTube, Netflix, VLC, mpv) from fullscreen games, VMs, and remote-desktop clients
//! so `auto_lock_fullscreen` can keep games locked while allowing seamless cursor crossing
//! past fullscreen video playback.

use crate::geometry::{DesktopGeometry, MonitorRect};

/// High-level category of a fullscreen foreground window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullscreenAppKind {
    /// Dedicated local/streaming video player (VLC, mpv, Kodi, Windows Media Player, etc.).
    VideoPlayer,
    /// Web browser (Chrome, Edge, Firefox, Brave, Zen, Vivaldi, etc.) — may play fullscreen
    /// videos or run pointer-locked browser games.
    Browser,
    /// Everything else (games, VMs, RDP/Parsec/Moonlight, IDEs, terminals, unknown/empty).
    Other,
}

#[inline]
fn is_video_player_exact(s: &str) -> bool {
    matches!(
        s,
        "vlc"
            | "mpv"
            | "mpv.net"
            | "mpvnet"
            | "mpc-hc"
            | "mpc-hc64"
            | "mpc-be"
            | "mpc-be64"
            | "potplayer"
            | "potplayer64"
            | "potplayermini"
            | "potplayermini64"
            | "kodi"
            | "plex"
            | "plexmediaplayer"
            | "plexhtpc"
            | "jellyfinmediaplayer"
            | "jellyfin-media-player"
            | "stremio"
            | "freetube"
            | "smplayer"
            | "wmplayer"
            | "microsoft.media.player"
            | "video.ui"
            | "zunevideo"
            | "appletv"
            | "netflix"
            | "primevideo"
            | "disneyplus"
            | "crunchyroll"
            | "celluloid"
            | "totem"
            | "showtime"
            | "haruna"
            | "clapper"
            | "dragonplayer"
            | "parole"
            | "bomi"
            | "hypnotix"
            | "iina"
            | "kmplayer"
            | "gom"
            | "gom64"
            | "bsplayer"
            | "screenbox"
            // Flatpak / reverse-DNS application IDs
            | "org.videolan.vlc"
            | "io.mpv.mpv"
            | "io.github.celluloid_player.celluloid"
            | "com.github.rafostar.clapper"
            | "org.kde.haruna"
            | "org.kde.dragonplayer"
            | "org.gnome.totem"
            | "org.gnome.showtime"
            | "io.freetubeapp.freetube"
            | "com.stremio.stremio"
            | "tv.kodi.kodi"
            | "tv.plex.plexdesktop"
            | "com.github.iwalton3.jellyfin-media-player"
    )
}

#[inline]
fn is_browser_exact(s: &str) -> bool {
    matches!(
        s,
        "chrome"
            | "google-chrome"
            | "google-chrome-stable"
            | "google-chrome-beta"
            | "google-chrome-unstable"
            | "chromium"
            | "chromium-browser"
            | "msedge"
            | "microsoft-edge"
            | "microsoft-edge-stable"
            | "microsoft-edge-beta"
            | "microsoft-edge-dev"
            | "firefox"
            | "firefox-esr"
            | "firefox-developer-edition"
            | "brave"
            | "brave-browser"
            | "vivaldi"
            | "vivaldi-stable"
            | "vivaldi-bin"
            | "opera"
            | "zen"
            | "zen-browser"
            | "arc"
            | "librewolf"
            | "floorp"
            | "waterfox"
            | "thorium"
            | "thorium-browser"
            | "helium"
            | "ungoogled-chromium"
            | "epiphany"
            | "falkon"
            | "qutebrowser"
            | "midori"
            // Flatpak / reverse-DNS application IDs
            | "com.brave.browser"
            | "org.mozilla.firefox"
            | "app.zen_browser.zen"
            | "com.google.chrome"
            | "com.microsoft.edge"
            | "org.chromium.chromium"
            | "io.gitlab.librewolf-community"
            | "one.ablaze.floorp"
            | "com.vivaldi.vivaldi"
            | "com.opera.opera"
    )
}

/// Unambiguous final dot-separated segment matches for reverse-DNS IDs not explicitly listed above
/// (deliberately excluding short/generic words like `browser`, `edge`, `arc`, `zen`, `dragon`).
#[inline]
fn is_unambiguous_video_player_segment(seg: &str) -> bool {
    matches!(
        seg,
        "vlc"
            | "mpv"
            | "mpvnet"
            | "mpc-hc"
            | "mpc-hc64"
            | "mpc-be"
            | "mpc-be64"
            | "potplayer"
            | "potplayer64"
            | "potplayermini"
            | "potplayermini64"
            | "kodi"
            | "plexmediaplayer"
            | "plexhtpc"
            | "plexdesktop"
            | "jellyfinmediaplayer"
            | "jellyfin-media-player"
            | "stremio"
            | "freetube"
            | "smplayer"
            | "wmplayer"
            | "zunevideo"
            | "celluloid"
            | "totem"
            | "haruna"
            | "clapper"
            | "hypnotix"
            | "iina"
            | "kmplayer"
            | "bsplayer"
            | "screenbox"
    )
}

#[inline]
fn is_unambiguous_browser_segment(seg: &str) -> bool {
    matches!(
        seg,
        "chrome"
            | "google-chrome"
            | "google-chrome-stable"
            | "google-chrome-beta"
            | "google-chrome-unstable"
            | "chromium"
            | "chromium-browser"
            | "msedge"
            | "microsoft-edge"
            | "microsoft-edge-stable"
            | "microsoft-edge-beta"
            | "microsoft-edge-dev"
            | "firefox"
            | "firefox-esr"
            | "firefox-developer-edition"
            | "brave"
            | "brave-browser"
            | "vivaldi"
            | "vivaldi-stable"
            | "vivaldi-bin"
            | "opera"
            | "zen-browser"
            | "librewolf"
            | "floorp"
            | "waterfox"
            | "thorium"
            | "thorium-browser"
            | "ungoogled-chromium"
            | "epiphany"
            | "falkon"
            | "qutebrowser"
            | "midori"
    )
}

/// Classifies a single OS application identifier (Windows executable name/stem, X11 `WM_CLASS`
/// instance or class token, or reverse-DNS Flatpak app ID).
pub fn classify_fullscreen_app(identifier: &str) -> FullscreenAppKind {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        return FullscreenAppKind::Other;
    }
    let lower = trimmed.to_ascii_lowercase();
    let normalized = lower.strip_suffix(".exe").unwrap_or(&lower);
    if normalized.is_empty() {
        return FullscreenAppKind::Other;
    }

    if is_video_player_exact(normalized) {
        return FullscreenAppKind::VideoPlayer;
    }
    if is_browser_exact(normalized) {
        return FullscreenAppKind::Browser;
    }

    if let Some((_, last)) = normalized.rsplit_once('.') {
        if is_unambiguous_video_player_segment(last) {
            return FullscreenAppKind::VideoPlayer;
        }
        if is_unambiguous_browser_segment(last) {
            return FullscreenAppKind::Browser;
        }
    }

    FullscreenAppKind::Other
}

/// Classifies multiple identifiers belonging to the same window (e.g. X11 `WM_CLASS`
/// `(instance, class)` pair), prioritizing [`FullscreenAppKind::VideoPlayer`] over
/// [`FullscreenAppKind::Browser`] over [`FullscreenAppKind::Other`].
pub fn classify_fullscreen_identifiers<'a>(
    ids: impl IntoIterator<Item = &'a str>,
) -> FullscreenAppKind {
    let mut best = FullscreenAppKind::Other;
    for id in ids {
        match classify_fullscreen_app(id) {
            FullscreenAppKind::VideoPlayer => return FullscreenAppKind::VideoPlayer,
            FullscreenAppKind::Browser => best = FullscreenAppKind::Browser,
            FullscreenAppKind::Other => {}
        }
    }
    best
}

/// Returns `true` when a Win32 `GetClipCursor` rectangle `(left, top, right, bottom)` indicates
/// active pointer confinement on `matched_monitor` (e.g. HTML5 Pointer Lock or an FPS camera
/// lock inside a fullscreen browser) rather than Windows' default unclipped virtual desktop rect.
pub fn is_cursor_rect_confined(
    clip: (i32, i32, i32, i32),
    matched_monitor: &MonitorRect,
    geom: &DesktopGeometry,
) -> bool {
    let (cl, ct, cr, cb) = clip;
    if cr <= cl || cb <= ct {
        return false;
    }

    const DESKTOP_TOL: i32 = 2;
    let (min_x, min_y, max_x, max_y) = geom.bounds();
    let covers_virtual_desktop = cl <= min_x.saturating_add(DESKTOP_TOL)
        && ct <= min_y.saturating_add(DESKTOP_TOL)
        && cr >= max_x.saturating_sub(DESKTOP_TOL)
        && cb >= max_y.saturating_sub(DESKTOP_TOL);
    if covers_virtual_desktop {
        return false;
    }

    // Active pointer lock / confinement restricts the cursor inside (or tightly around)
    // the fullscreen monitor (e.g. a 1x1 center rect, viewport rect, or single monitor on
    // a multi-monitor desktop).
    const MONITOR_TOL: i32 = 16;
    cl >= matched_monitor.left().saturating_sub(MONITOR_TOL)
        && ct >= matched_monitor.top().saturating_sub(MONITOR_TOL)
        && cr <= matched_monitor.right().saturating_add(MONITOR_TOL)
        && cb <= matched_monitor.bottom().saturating_add(MONITOR_TOL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_windows_video_players_and_browsers_case_insensitively() {
        for player in [
            "vlc.exe",
            "VLC.EXE",
            "mpv.exe",
            "mpv.net.exe",
            "mpvnet.exe",
            "mpc-hc64.exe",
            "MPC-BE64.EXE",
            "PotPlayer64.exe",
            "PotPlayerMini64.exe",
            "Microsoft.Media.Player.exe",
            "Video.UI.exe",
            "ZuneVideo.exe",
            "AppleTV.exe",
            "wmplayer.exe",
            "kodi.exe",
            "PlexMediaPlayer.exe",
            "PlexHTPC.exe",
            "JellyfinMediaPlayer.exe",
            "stremio.exe",
            "FreeTube.exe",
            "Netflix.exe",
            "PrimeVideo.exe",
            "DisneyPlus.exe",
            "Crunchyroll.exe",
            "Screenbox.exe",
            "  vlc.exe  ",
        ] {
            assert_eq!(
                classify_fullscreen_app(player),
                FullscreenAppKind::VideoPlayer,
                "expected VideoPlayer for '{player}'"
            );
        }

        for browser in [
            "chrome.exe",
            "CHROME.EXE",
            "msedge.exe",
            "firefox.exe",
            "brave.exe",
            "vivaldi.exe",
            "opera.exe",
            "zen.exe",
            "Arc.exe",
            "librewolf.exe",
            "floorp.exe",
            "waterfox.exe",
            "thorium.exe",
            "helium.exe",
        ] {
            assert_eq!(
                classify_fullscreen_app(browser),
                FullscreenAppKind::Browser,
                "expected Browser for '{browser}'"
            );
        }
    }

    #[test]
    fn classifies_flatpak_and_reverse_dns_ids_without_false_positives() {
        for player_id in [
            "org.videolan.vlc",
            "org.videolan.VLC",
            "io.mpv.mpv",
            "io.github.celluloid_player.celluloid",
            "com.github.rafostar.clapper",
            "org.kde.haruna",
            "org.gnome.totem",
            "org.gnome.showtime",
            "io.freetubeapp.freetube",
            "com.stremio.stremio",
            "tv.kodi.kodi",
            "tv.plex.plexdesktop",
            "com.github.iwalton3.jellyfin-media-player",
            "org.custom.vlc",
        ] {
            assert_eq!(
                classify_fullscreen_app(player_id),
                FullscreenAppKind::VideoPlayer,
                "expected VideoPlayer for '{player_id}'"
            );
        }

        for browser_id in [
            "com.brave.browser",
            "org.mozilla.firefox",
            "app.zen_browser.zen",
            "com.google.chrome",
            "com.microsoft.edge",
            "org.chromium.chromium",
            "io.gitlab.librewolf-community",
            "one.ablaze.floorp",
            "com.vivaldi.vivaldi",
            "com.opera.opera",
            "org.gnome.Epiphany",
            "org.kde.falkon",
            "org.qutebrowser.qutebrowser",
        ] {
            assert_eq!(
                classify_fullscreen_app(browser_id),
                FullscreenAppKind::Browser,
                "expected Browser for '{browser_id}'"
            );
        }

        // Generic trailing segments must NOT match unrelated apps/games
        for non_match in [
            "com.example.browser",
            "com.studio.edge",
            "com.studio.arc",
            "com.studio.zen",
            "com.studio.dragon",
            "com.example.player",
        ] {
            assert_eq!(
                classify_fullscreen_app(non_match),
                FullscreenAppKind::Other,
                "generic reverse-DNS ID '{non_match}' must stay Other"
            );
        }
    }

    #[test]
    fn classifies_x11_wm_class_pairs_and_respects_priority() {
        // Chrome YouTube PWA on X11: instance is crx_..., class is Google-chrome
        assert_eq!(
            classify_fullscreen_identifiers(["crx_agimnkijcaahngcdmfeangaknmldooml", "Google-chrome"]),
            FullscreenAppKind::Browser
        );
        // Firefox on X11
        assert_eq!(
            classify_fullscreen_identifiers(["Navigator", "firefox"]),
            FullscreenAppKind::Browser
        );
        assert_eq!(
            classify_fullscreen_identifiers(["Navigator", "firefox-esr"]),
            FullscreenAppKind::Browser
        );
        // mpv on X11 (instance "gl", class "mpv")
        assert_eq!(
            classify_fullscreen_identifiers(["gl", "mpv"]),
            FullscreenAppKind::VideoPlayer
        );
        // Priority: VideoPlayer wins over Browser and Other
        assert_eq!(
            classify_fullscreen_identifiers(["unknown", "chrome", "vlc"]),
            FullscreenAppKind::VideoPlayer
        );
        assert_eq!(
            classify_fullscreen_identifiers(["unknown", "brave-browser"]),
            FullscreenAppKind::Browser
        );
        assert_eq!(
            classify_fullscreen_identifiers(["steam_app_1091500", "Cyberpunk2077.exe"]),
            FullscreenAppKind::Other
        );
    }

    #[test]
    fn classifies_games_vms_rdp_ides_and_empty_as_other() {
        for other in [
            "",
            "   ",
            ".exe",
            "cs2.exe",
            "VALORANT-Win64-Shipping.exe",
            "eldenring.exe",
            "Cyberpunk2077.exe",
            "Dragon.exe",
            "Navigator.exe",
            "UnrealEngine.exe",
            "UnityPlayer.exe",
            "steam.exe",
            "vmware.exe",
            "VirtualBoxVM.exe",
            "qemu-system-x86_64",
            "mstsc.exe",
            "parsecd.exe",
            "Moonlight.exe",
            "Code.exe",
            "Discord.exe",
            "WindowsTerminal.exe",
            "Alacritty",
            "kitty",
        ] {
            assert_eq!(
                classify_fullscreen_app(other),
                FullscreenAppKind::Other,
                "expected Other for '{other}'"
            );
        }
    }

    #[test]
    fn cursor_rect_confinement_distinguishes_unclipped_desktop_from_pointer_lock() {
        let single_mon = MonitorRect {
            name: "DP-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        };
        let single_geom = DesktopGeometry::new(vec![single_mon.clone()]);

        // Unclipped single monitor (exact and within 2px tolerance)
        assert!(!is_cursor_rect_confined((0, 0, 1920, 1080), &single_mon, &single_geom));
        assert!(!is_cursor_rect_confined((0, 0, 1919, 1079), &single_mon, &single_geom));
        // Degenerate / empty clip rect is ignored safely
        assert!(!is_cursor_rect_confined((0, 0, 0, 0), &single_mon, &single_geom));
        assert!(!is_cursor_rect_confined((100, 100, 100, 100), &single_mon, &single_geom));
        // 1x1 HTML5 Pointer Lock rect in center of browser window -> CONFINED
        assert!(is_cursor_rect_confined((960, 540, 961, 541), &single_mon, &single_geom));
        // Inset viewport clip -> CONFINED
        assert!(is_cursor_rect_confined((20, 40, 1900, 1040), &single_mon, &single_geom));

        // Dual monitor setup
        let left_mon = MonitorRect {
            name: "DP-1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: true,
        };
        let right_mon = MonitorRect {
            name: "DP-2".into(),
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
            scale: 1.5,
            primary: false,
        };
        let dual_geom = DesktopGeometry::new(vec![left_mon.clone(), right_mon.clone()]);

        // Unclipped virtual desktop spans both monitors -> NOT confined
        assert!(!is_cursor_rect_confined((0, 0, 4480, 1440), &left_mon, &dual_geom));
        assert!(!is_cursor_rect_confined((0, 0, 4480, 1440), &right_mon, &dual_geom));

        // Browser game confining cursor to the fullscreen monitor -> CONFINED
        assert!(is_cursor_rect_confined((0, 0, 1920, 1080), &left_mon, &dual_geom));
        assert!(is_cursor_rect_confined((1920, 0, 4480, 1440), &right_mon, &dual_geom));
        // 1x1 Pointer Lock on right monitor -> CONFINED
        assert!(is_cursor_rect_confined((3200, 720, 3201, 721), &right_mon, &dual_geom));
        // Clip confined to left monitor does not falsely match right monitor
        assert!(!is_cursor_rect_confined((0, 0, 1920, 1080), &right_mon, &dual_geom));

        // Negative-coordinate multi-monitor desktop (secondary monitor to the left & above primary)
        let neg_left = MonitorRect {
            name: "DP-Left".into(),
            x: -1920,
            y: -200,
            width: 1920,
            height: 1080,
            scale: 1.0,
            primary: false,
        };
        let neg_primary = MonitorRect {
            name: "DP-Primary".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            scale: 1.0,
            primary: true,
        };
        let neg_geom = DesktopGeometry::new(vec![neg_left.clone(), neg_primary.clone()]);

        // Unclipped virtual desktop spans (-1920, -200) .. (2560, 1440) -> NOT confined
        assert!(!is_cursor_rect_confined(
            (-1920, -200, 2560, 1440),
            &neg_left,
            &neg_geom
        ));
        // Confined to negative-coordinate monitor -> CONFINED
        assert!(is_cursor_rect_confined(
            (-1920, -200, 0, 880),
            &neg_left,
            &neg_geom
        ));
        // 1x1 Pointer Lock inside negative-coordinate monitor -> CONFINED
        assert!(is_cursor_rect_confined(
            (-960, 340, -959, 341),
            &neg_left,
            &neg_geom
        ));
        // Clip on primary does not match negative left monitor
        assert!(!is_cursor_rect_confined(
            (0, 0, 2560, 1440),
            &neg_left,
            &neg_geom
        ));
    }
}

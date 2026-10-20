//! The menu bar shell's decisions, separated from the platform calls that
//! act on them.
//!
//! Every bug that reached the user so far lived here rather than in the
//! tracking core: the popover toggling twice per click, dismissing itself
//! the instant it opened, and reopening when its icon was clicked to close
//! it. None was reachable by a test while the logic sat inside the event
//! loop. These functions are the same rules, made checkable.

use std::time::{Duration, Instant};

/// Event-loop wake interval while the popover is open, so clicks feel instant.
pub const AWAKE_POLL: Duration = Duration::from_millis(120);
/// …and while it is closed, where only the sampling tick matters.
pub const IDLE_POLL: Duration = Duration::from_secs(5);
/// Focus loss within this window of opening is opening churn, not dismissal.
pub const FOCUS_GRACE: Duration = Duration::from_millis(400);
/// A press on the icon this soon after focus loss hid the popover is the
/// press that took the focus: a click meant to close it.
pub const CLOSE_CLICK: Duration = Duration::from_millis(300);
/// Gap between the menu bar and the popover's top edge.
pub const POPOVER_GAP: f64 = 4.0;
/// Keep the popover clear of the screen edges.
pub const SCREEN_MARGIN: f64 = 8.0;

/// Which mouse button and state a tray event carries, mirrored from
/// `tray_icon` so this module does not depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Up,
    Down,
}

/// What a tray click does to the popover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Show,
    Hide,
}

/// Turns tray clicks into show and hide.
///
/// `tray_icon` emits a `Click` for **both** press and release, so reacting to
/// every one toggles twice per physical click. Only the left button's
/// release acts. Whether it shows or hides is decided at the press: pressing
/// the icon of an open popover takes its focus, and focus loss may hide it
/// before the release arrives. Deciding at the release would see it hidden
/// and open it again.
#[derive(Debug, Default)]
pub struct TrayClicks {
    closing: bool,
    dismissed_at: Option<Instant>,
}

impl TrayClicks {
    /// Focus loss hid the popover.
    pub fn dismissed(&mut self, now: Instant) {
        self.dismissed_at = Some(now);
    }

    pub fn click(
        &mut self,
        button: Button,
        state: ButtonState,
        visible: bool,
        now: Instant,
    ) -> Option<Toggle> {
        if button != Button::Left {
            return None;
        }
        match state {
            ButtonState::Down => {
                let just_hidden = self
                    .dismissed_at
                    .is_some_and(|t| now.saturating_duration_since(t) < CLOSE_CLICK);
                self.closing = visible || just_hidden;
                None
            }
            // A release without its press (none seen) toggles as before.
            ButtonState::Up => Some(if std::mem::take(&mut self.closing) || visible {
                Toggle::Hide
            } else {
                Toggle::Show
            }),
        }
    }
}

/// Should focus loss dismiss the popover?
///
/// Showing a window generates transient focus churn, and an `Accessory` app's
/// window is not key until the app itself is activated. Dismissing during that
/// window is what made the popover flash and disappear.
pub fn should_dismiss(visible: bool, since_shown: Duration) -> bool {
    visible && since_shown > FOCUS_GRACE
}

pub fn poll_interval(visible: bool) -> Duration {
    if visible { AWAKE_POLL } else { IDLE_POLL }
}

/// A display, in the physical pixel coordinates the tray icon reports in.
///
/// Mirrored from `tao::monitor::MonitorHandle` so this module — and its
/// tests — do not need a window system.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Backing-scale factor. The popover's size is configured in logical
    /// points, but every coordinate here is physical, so the two only agree
    /// once multiplied by this.
    pub scale: f64,
}

/// Which display the tray icon is on.
///
/// The click can land on any screen, so the popover has to be clamped to the
/// screen the ICON is on — not the one the window happens to be sitting on
/// from last time. `current_monitor()` reports the latter, which is why
/// opening from the external display could place the popover half onto the
/// laptop: the clamp was being applied against the wrong rectangle.
///
/// Matching is on the icon's centre point rather than its origin, so an icon
/// straddling a seam still resolves to the screen it is mostly on. If nothing
/// contains it — screens can be arranged with gaps — the nearest by centre
/// distance wins, which is always better than defaulting to the primary.
pub fn screen_for(
    screens: &[Screen],
    tray_x: f64,
    tray_y: f64,
    tray_w: f64,
    tray_h: f64,
) -> Option<Screen> {
    let (cx, cy) = (tray_x + tray_w / 2.0, tray_y + tray_h / 2.0);
    let contains = |s: &Screen| cx >= s.x && cx < s.x + s.w && cy >= s.y && cy < s.y + s.h;
    screens
        .iter()
        .find(|s| contains(s))
        .or_else(|| {
            // Distance to the nearest EDGE, not to the centre: a point just
            // below a wide screen is nearest to that screen, however far its
            // middle happens to be.
            screens.iter().min_by(|a, b| {
                let d = |s: &Screen| {
                    let dx = (s.x - cx).max(cx - (s.x + s.w)).max(0.0);
                    let dy = (s.y - cy).max(cy - (s.y + s.h)).max(0.0);
                    dx * dx + dy * dy
                };
                d(a).total_cmp(&d(b))
            })
        })
        .copied()
}

/// Where to put the popover's top-left corner, given the tray icon's rect.
///
/// Centred under the icon, then clamped to the screen. The clamp matters on a
/// wide display: an icon near the right edge would otherwise place most of the
/// popover past the end of the screen.
#[allow(clippy::too_many_arguments)]
pub fn popover_origin(
    tray_x: f64,
    tray_y: f64,
    tray_w: f64,
    tray_h: f64,
    popover_w: f64,
    popover_h: f64,
    screen_x: f64,
    screen_y: f64,
    screen_w: f64,
    screen_h: f64,
) -> (f64, f64) {
    let x = tray_x + tray_w / 2.0 - popover_w / 2.0;
    let y = tray_y + tray_h + POPOVER_GAP;

    let min_x = screen_x + SCREEN_MARGIN;
    let max_x = (screen_x + screen_w - popover_w - SCREEN_MARGIN).max(min_x);
    let min_y = screen_y + SCREEN_MARGIN;
    let max_y = (screen_y + screen_h - popover_h - SCREEN_MARGIN).max(min_y);

    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

/// The menu bar glyph: four bars, `filled` of them solid, the rest faint.
/// Drawn in code so the app ships no image assets. `ink` is `None` under
/// normal pressure, where the image is used as a template and follows the
/// menu bar's light or dark appearance; amber or red otherwise.
pub fn memory_glyph_rgba(size: u32, filled: u32, ink: Option<[u8; 3]>) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let [r, g, b] = ink.unwrap_or([0, 0, 0]);
    // Four bars with gaps, centred; a 2px margin top and bottom.
    let bar = (size - 2) / 4;
    let w = bar.saturating_sub(1).max(1);
    let left = (size - bar * 4).div_ceil(2);
    for i in 0..4 {
        let a = if i < filled { 255 } else { 70 };
        let x0 = left + i * bar;
        for y in 2..size - 2 {
            for x in x0..(x0 + w).min(size) {
                let p = ((y * size + x) * 4) as usize;
                rgba[p..p + 4].copy_from_slice(&[r, g, b, a]);
            }
        }
    }
    rgba
}

/// How many of the four bars to fill for this much memory in use.
pub fn glyph_segments(used: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    ((used as f64 / total as f64 * 4.0).round() as u32).min(4)
}

// ── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const L: Button = Button::Left;
    const DOWN: ButtonState = ButtonState::Down;
    const UP: ButtonState = ButtonState::Up;
    const MS: fn(u64) -> Duration = Duration::from_millis;

    /// Regression: one physical click used to toggle twice, so the popover
    /// opened on press and closed again on release.
    #[test]
    fn a_click_toggles_exactly_once() {
        let (mut c, t) = (TrayClicks::default(), Instant::now());
        assert_eq!(c.click(L, DOWN, false, t), None);
        assert_eq!(c.click(L, UP, false, t + MS(80)), Some(Toggle::Show));
        assert_eq!(c.click(L, DOWN, true, t + MS(2000)), None);
        assert_eq!(c.click(L, UP, true, t + MS(2080)), Some(Toggle::Hide));
    }

    #[test]
    fn other_buttons_do_not_toggle() {
        let (mut c, t) = (TrayClicks::default(), Instant::now());
        for b in [Button::Right, Button::Middle] {
            assert_eq!(c.click(b, DOWN, false, t), None);
            assert_eq!(c.click(b, UP, false, t), None);
        }
    }

    /// Regression: clicking the icon to close the popover sometimes opened
    /// it again. The press took the popover's focus, focus loss hid it, and
    /// the release then saw it hidden.
    #[test]
    fn a_closing_click_stays_closed_whichever_event_comes_first() {
        let t = Instant::now();
        // Press first, then focus loss hides it, then the release.
        let mut c = TrayClicks::default();
        c.click(L, DOWN, true, t);
        c.dismissed(t + MS(5));
        assert_eq!(c.click(L, UP, false, t + MS(90)), Some(Toggle::Hide));
        // Focus loss first: by the press it is already hidden.
        let mut c = TrayClicks::default();
        c.dismissed(t);
        c.click(L, DOWN, false, t + MS(10));
        assert_eq!(c.click(L, UP, false, t + MS(90)), Some(Toggle::Hide));
    }

    #[test]
    fn a_click_after_clicking_elsewhere_opens_it() {
        let t = Instant::now();
        let mut c = TrayClicks::default();
        c.dismissed(t);
        c.click(L, DOWN, false, t + CLOSE_CLICK - MS(1));
        assert_eq!(c.click(L, UP, false, t + CLOSE_CLICK), Some(Toggle::Hide));
        c.click(L, DOWN, false, t + CLOSE_CLICK);
        assert_eq!(
            c.click(L, UP, false, t + CLOSE_CLICK + MS(80)),
            Some(Toggle::Show)
        );
        // The next click closes it again: nothing carried over.
        c.click(L, DOWN, true, t + MS(5000));
        assert_eq!(c.click(L, UP, true, t + MS(5080)), Some(Toggle::Hide));
    }

    #[test]
    fn a_release_without_a_press_toggles() {
        let t = Instant::now();
        assert_eq!(
            TrayClicks::default().click(L, UP, false, t),
            Some(Toggle::Show)
        );
        assert_eq!(
            TrayClicks::default().click(L, UP, true, t),
            Some(Toggle::Hide)
        );
    }

    /// Regression: the popover dismissed itself during the focus churn of
    /// opening, because an accessory app's window is not key straight away.
    #[test]
    fn focus_loss_while_opening_is_ignored() {
        assert!(!should_dismiss(true, Duration::from_millis(0)));
        assert!(!should_dismiss(true, Duration::from_millis(399)));
        assert!(should_dismiss(true, Duration::from_millis(401)));
    }

    #[test]
    fn a_hidden_popover_is_never_dismissed_again() {
        assert!(!should_dismiss(false, Duration::from_secs(10)));
    }

    #[test]
    fn polling_is_lazy_while_closed_and_responsive_while_open() {
        assert!(poll_interval(true) < poll_interval(false));
        assert_eq!(poll_interval(true), AWAKE_POLL);
    }

    // ── positioning ───────────────────────────────────────────────────────

    const W: f64 = 380.0;
    const H: f64 = 545.0;

    fn origin_on_ultrawide(tray_x: f64) -> (f64, f64) {
        // The display this was built against.
        popover_origin(tray_x, 0.0, 24.0, 24.0, W, H, 0.0, 0.0, 5120.0, 2160.0)
    }

    #[test]
    fn the_popover_is_centred_under_the_tray_icon() {
        let (x, y) = origin_on_ultrawide(2000.0);
        assert_eq!(x, 2000.0 + 12.0 - W / 2.0);
        assert_eq!(y, 24.0 + POPOVER_GAP, "just below the menu bar");
    }

    /// An icon near the right edge would otherwise put most of the popover
    /// off the end of the screen — easy to miss on a wide display.
    #[test]
    fn it_is_pulled_back_from_the_right_edge() {
        let (x, _) = origin_on_ultrawide(5100.0);
        assert!(x + W <= 5120.0 - SCREEN_MARGIN + 0.001, "x was {x}");
    }

    #[test]
    fn it_is_pushed_off_the_left_edge_too() {
        let (x, _) = origin_on_ultrawide(2.0);
        assert!(x >= SCREEN_MARGIN - 0.001, "x was {x}");
    }

    /// Multi-monitor: a screen whose origin is not (0,0) must be respected.
    #[test]
    fn it_stays_on_a_secondary_display() {
        let (x, y) = popover_origin(
            -1400.0, -200.0, 24.0, 24.0, W, H, -1512.0, -200.0, 1512.0, 982.0,
        );
        assert!(x >= -1512.0 + SCREEN_MARGIN - 0.001, "x was {x}");
        assert!(
            x + W <= -1512.0 + 1512.0 - SCREEN_MARGIN + 0.001,
            "x was {x}"
        );
        assert!(y >= -200.0, "y was {y}");
    }

    // ── which display ─────────────────────────────────────────────────────
    //
    // A laptop with an external display above it, both 2×. The external is
    // the primary at (0,0); the laptop sits below and to the right, which is
    // a common arrangement and the one that exposed the bug.
    const EXTERNAL: Screen = Screen {
        x: 0.0,
        y: 0.0,
        w: 5120.0,
        h: 2160.0,
        scale: 2.0,
    };
    const LAPTOP: Screen = Screen {
        x: 1800.0,
        y: 2160.0,
        w: 3024.0,
        h: 1964.0,
        scale: 2.0,
    };

    /// The popover must be clamped to the screen the ICON is on. Reading the
    /// window's own monitor instead gave whichever display it was last opened
    /// on, which is what put half the popover on the laptop.
    #[test]
    fn the_icon_decides_which_screen_the_popover_belongs_to() {
        let screens = [EXTERNAL, LAPTOP];
        assert_eq!(
            screen_for(&screens, 4000.0, 0.0, 24.0, 24.0),
            Some(EXTERNAL)
        );
        assert_eq!(
            screen_for(&screens, 4000.0, 2160.0, 24.0, 24.0),
            Some(LAPTOP)
        );
    }

    /// Matched on the icon's centre, so an icon straddling the seam resolves
    /// to the screen it is mostly on rather than to neither.
    #[test]
    fn an_icon_on_the_seam_lands_on_one_screen() {
        let screens = [EXTERNAL, LAPTOP];
        // Its left edge is on the external, its centre on the laptop.
        let s = screen_for(&screens, 1790.0, 2160.0, 24.0, 24.0);
        assert_eq!(s, Some(LAPTOP));
    }

    /// Screens can be arranged with gaps between them. Falling back to the
    /// primary would send the popover to the wrong display entirely; the
    /// nearest screen is always the better guess.
    #[test]
    fn an_icon_in_a_gap_falls_back_to_the_nearest_screen() {
        let screens = [EXTERNAL, LAPTOP];
        // Off the bottom-right of the external, just past the laptop's right
        // edge — nearest the laptop by a long way.
        let s = screen_for(&screens, 4900.0, 3000.0, 24.0, 24.0);
        assert_eq!(s, Some(LAPTOP));
        // And a point below the external but far to its left belongs to it,
        // not to the laptop off to the right.
        assert_eq!(
            screen_for(&screens, 200.0, 3000.0, 24.0, 24.0),
            Some(EXTERNAL)
        );
    }

    #[test]
    fn no_screens_at_all_yields_nothing_rather_than_a_guess() {
        assert_eq!(screen_for(&[], 0.0, 0.0, 24.0, 24.0), None);
    }

    /// The tray rect and the screen bounds are physical pixels; the popover's
    /// size is configured in logical points. On a 2× display the clamp saw a
    /// popover half the real width, so an icon near the right edge was
    /// "corrected" to a position that still overran by ~380px — onto whatever
    /// display was next along.
    #[test]
    fn the_popover_is_clamped_at_its_physical_width() {
        let s = EXTERNAL;
        let icon_x = s.x + s.w - 40.0; // hard against the right edge
        let logical = popover_origin(icon_x, 0.0, 24.0, 24.0, W, H, s.x, s.y, s.w, s.h);
        let physical = popover_origin(
            icon_x,
            0.0,
            24.0,
            24.0,
            W * s.scale,
            H * s.scale,
            s.x,
            s.y,
            s.w,
            s.h,
        );
        assert!(
            logical.0 + W * s.scale > s.x + s.w,
            "the logical width overruns the screen — this was the bug"
        );
        assert!(
            physical.0 + W * s.scale <= s.x + s.w - SCREEN_MARGIN + 0.001,
            "x was {}",
            physical.0
        );
    }

    /// A screen too small for the popover must still yield a usable origin
    /// rather than an inverted clamp range.
    #[test]
    fn a_screen_smaller_than_the_popover_does_not_panic() {
        let (x, y) = popover_origin(10.0, 0.0, 24.0, 24.0, W, H, 0.0, 0.0, 200.0, 200.0);
        assert!(x.is_finite() && y.is_finite());
        assert!(x >= SCREEN_MARGIN - 0.001);
    }

    // ── glyph ─────────────────────────────────────────────────────────────

    fn alpha_at(rgba: &[u8], size: u32, x: u32, y: u32) -> u8 {
        rgba[((y * size + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_glyph_fills_bars_left_to_right() {
        let size = 18;
        let g = memory_glyph_rgba(size, 3, None);
        assert_eq!(g.len(), (size * size * 4) as usize);
        let bar = (size - 2) / 4;
        let left = (size - bar * 4).div_ceil(2);
        let mid = size / 2;
        let alphas: Vec<u8> = (0..4)
            .map(|i| alpha_at(&g, size, left + i * bar, mid))
            .collect();
        assert_eq!(alphas, vec![255, 255, 255, 70]);
        assert_eq!(alpha_at(&g, size, 0, 0), 0, "transparent corner");
    }

    #[test]
    fn the_glyph_takes_its_colour_only_when_given_one() {
        let g = memory_glyph_rgba(18, 4, Some([239, 68, 68]));
        let p = g.chunks(4).find(|p| p[3] == 255).unwrap();
        assert_eq!(&p[..3], &[239, 68, 68]);
        let t = memory_glyph_rgba(18, 4, None);
        assert!(
            t.chunks(4)
                .filter(|p| p[3] > 0)
                .all(|p| p[..3] == [0, 0, 0])
        );
    }

    #[test]
    fn segments_round_to_the_nearest_quarter() {
        assert_eq!(glyph_segments(0, 36), 0);
        assert_eq!(glyph_segments(27, 36), 3);
        assert_eq!(glyph_segments(20, 24), 3);
        assert_eq!(glyph_segments(36, 36), 4);
        assert_eq!(glyph_segments(99, 36), 4);
        assert_eq!(
            glyph_segments(5, 0),
            0,
            "unknown total draws empty, not a panic"
        );
    }
}

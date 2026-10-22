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
/// Defer focus-loss dismissal while the window is opening.
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

/// Keep early focus loss pending until opening churn has settled. Regaining
/// focus or explicitly hiding the window cancels any pending dismissal.
#[derive(Debug, Default)]
pub struct FocusDismissal {
    shown_at: Option<Instant>,
    deadline: Option<Instant>,
}

impl FocusDismissal {
    pub fn shown(&mut self, now: Instant) {
        self.shown_at = Some(now);
        self.deadline = None;
    }

    pub fn hidden(&mut self) {
        self.shown_at = None;
        self.deadline = None;
    }

    pub fn changed(&mut self, focused: bool, now: Instant) {
        self.deadline = if focused {
            None
        } else {
            self.shown_at.map(|shown| (shown + FOCUS_GRACE).max(now))
        };
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Check the native window's current focus before acting on the timer.
    pub fn dismiss_if_due(&mut self, now: Instant, focused: bool) -> bool {
        if !self.deadline.is_some_and(|deadline| now >= deadline) {
            return false;
        }
        self.deadline = None;
        if focused {
            return false;
        }
        self.hidden();
        true
    }
}

pub fn poll_interval(visible: bool) -> Duration {
    if visible { AWAKE_POLL } else { IDLE_POLL }
}

/// A rectangle in desktop logical points, with Y increasing downward.
/// AppKit rectangles are reflected around Y=0; no screen scale is involved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Screen {
    /// Convert a native bottom-left rectangle to our top-left coordinates.
    /// The reflection is shared by every display, including those above or
    /// to the left of the primary. Convert the final top-left Y back with -Y.
    pub fn from_cocoa(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y: -y - h, w, h }
    }
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

    #[test]
    fn early_focus_loss_is_deferred_and_then_dismissed() {
        let t = Instant::now();
        let mut focus = FocusDismissal::default();
        focus.shown(t);
        focus.changed(false, t + MS(50));
        assert_eq!(focus.deadline(), Some(t + FOCUS_GRACE));
        assert!(!focus.dismiss_if_due(t + FOCUS_GRACE - MS(1), false));
        assert!(focus.dismiss_if_due(t + FOCUS_GRACE, false));
        assert_eq!(focus.deadline(), None);
        assert!(!focus.dismiss_if_due(t + MS(1000), false));
    }

    #[test]
    fn regaining_focus_cancels_the_pending_dismissal() {
        let t = Instant::now();
        let mut focus = FocusDismissal::default();
        focus.shown(t);
        focus.changed(false, t + MS(50));
        focus.changed(true, t + MS(100));
        assert_eq!(focus.deadline(), None);
        assert!(!focus.dismiss_if_due(t + FOCUS_GRACE, false));
        // Even if its notification is still queued, native focus wins.
        focus.changed(false, t + MS(200));
        assert!(!focus.dismiss_if_due(t + FOCUS_GRACE, true));
        assert_eq!(focus.deadline(), None);
    }

    #[test]
    fn hiding_cancels_the_timer_and_reopening_gets_a_new_grace_period() {
        let t = Instant::now();
        let mut focus = FocusDismissal::default();
        focus.shown(t);
        focus.changed(false, t + MS(50));
        focus.hidden();
        focus.changed(false, t + MS(150));
        assert_eq!(focus.deadline(), None);
        assert!(!focus.dismiss_if_due(t + FOCUS_GRACE, false));
        focus.shown(t + MS(1000));
        focus.changed(false, t + MS(1050));
        assert_eq!(focus.deadline(), Some(t + MS(1000) + FOCUS_GRACE));
    }

    #[test]
    fn focus_loss_after_opening_dismisses_immediately() {
        let t = Instant::now();
        let mut focus = FocusDismissal::default();
        focus.shown(t);
        focus.changed(false, t + MS(1000));
        assert_eq!(focus.deadline(), Some(t + MS(1000)));
        assert!(focus.dismiss_if_due(t + MS(1000), false));
    }

    #[test]
    fn a_tray_close_during_grace_does_not_reopen_after_deferred_dismissal() {
        let t = Instant::now();
        let mut focus = FocusDismissal::default();
        let mut clicks = TrayClicks::default();
        focus.shown(t);
        focus.changed(false, t + MS(350));
        clicks.click(L, DOWN, true, t + MS(360));
        assert!(focus.dismiss_if_due(t + FOCUS_GRACE, false));
        clicks.dismissed(t + FOCUS_GRACE);
        assert_eq!(clicks.click(L, UP, false, t + MS(450)), Some(Toggle::Hide));
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

    // Native rectangles are points on every display, irrespective of 1x/2x
    // backing. A 1512x982 Retina laptop and 1920x1080 external display use
    // these same dimensions in both directions of a window move.
    fn native_origin(tray: Screen, visible: Screen) -> (f64, f64) {
        let (x, y) = popover_origin(
            tray.x, tray.y, tray.w, tray.h, 400.0, 600.0, visible.x, visible.y, visible.w,
            visible.h,
        );
        (x, -y)
    }

    #[test]
    fn mixed_scale_displays_share_logical_desktop_coordinates() {
        let laptop = Screen::from_cocoa(0.0, 0.0, 1512.0, 958.0);
        let external = Screen::from_cocoa(1512.0, 0.0, 1920.0, 1056.0);
        let laptop_tray = Screen::from_cocoa(1472.0, 958.0, 24.0, 24.0);
        let external_tray = Screen::from_cocoa(3392.0, 1056.0, 24.0, 24.0);
        // Clamp at the real logical width, independent of the old display.
        for (tray, visible, expected) in [
            (laptop_tray, laptop, (1104.0, 950.0)),
            (external_tray, external, (3024.0, 1048.0)),
            (laptop_tray, laptop, (1104.0, 950.0)),
        ] {
            assert_eq!(native_origin(tray, visible), expected);
        }
    }

    #[test]
    fn displays_above_and_left_keep_their_native_origins() {
        let above = Screen::from_cocoa(-300.0, 982.0, 1920.0, 1056.0);
        let above_tray = Screen::from_cocoa(-290.0, 2038.0, 24.0, 24.0);
        assert_eq!(native_origin(above_tray, above), (-292.0, 2030.0));
        let left = Screen::from_cocoa(-1920.0, -200.0, 1920.0, 1056.0);
        let left_tray = Screen::from_cocoa(-40.0, 856.0, 24.0, 24.0);
        assert_eq!(native_origin(left_tray, left), (-408.0, 848.0));
    }

    #[test]
    fn visible_screen_bounds_keep_the_popover_clear_of_the_dock() {
        // A dock on the right takes 80 points; bottom dock takes 70 points.
        let visible = Screen::from_cocoa(0.0, 70.0, 1432.0, 700.0);
        let tray = Screen::from_cocoa(1480.0, 770.0, 24.0, 24.0);
        let (x, top) = native_origin(tray, visible);
        assert_eq!((x, top), (1024.0, 762.0));
        assert!(top - 600.0 >= 70.0 + SCREEN_MARGIN);
        assert!(x + 400.0 <= 1432.0 - SCREEN_MARGIN);
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

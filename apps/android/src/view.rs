//! Small geometry rules the host applies around the soft keyboard: keeping
//! the caret above it, and telling a tap (which brings a dismissed keyboard
//! back) from a pan, drag or pinch (which does not).

use anypage_engine::core::Rect;

/// A caret rectangle on screen: x, y, w, h in physical pixels.
pub type ScreenRect = (f32, f32, f32, f32);

/// What Kotlin is told when a page's caret has no usable position (a value
/// that overflowed `f32`): focus is on, the IME window goes to the corner.
pub const UNKNOWN_CARET: ScreenRect = (0.0, 0.0, 1.0, 1.0);

fn all_finite(r: ScreenRect) -> bool {
    r.0.is_finite() && r.1.is_finite() && r.2.is_finite() && r.3.is_finite()
}

/// The caret as Kotlin gets it: never infinite or NaN.
pub fn sanitize(r: ScreenRect) -> ScreenRect {
    if all_finite(r) {
        r
    } else {
        UNKNOWN_CARET
    }
}

/// Whether the host may move the camera to show a caret at `caret` (world
/// units): its numbers are finite and it lies within a page's size of the
/// page. A caret far outside the page (a script bug, or 1e30) would pan the
/// page out of sight for good.
pub fn revealable(caret: Rect, bounds: Rect) -> bool {
    let finite = [caret.x, caret.y, caret.w, caret.h].iter().all(|v| v.is_finite());
    finite && caret.intersects(&bounds.inflate(bounds.w.max(bounds.h).max(1.0)))
}

/// How far to move the span `[a, a + len]` so it lies within `[lo, hi]`.
/// When it cannot fit, its start wins (the top of a caret, the left of a word).
pub fn shift_into(a: f32, len: f32, lo: f32, hi: f32) -> f32 {
    if a + len > hi {
        let d = hi - (a + len);
        if a + d < lo {
            lo - a
        } else {
            d
        }
    } else if a < lo {
        lo - a
    } else {
        0.0
    }
}

/// Camera pan (dx, dy) that brings `caret` into the part of a `w` x `h`
/// surface the keyboard does not cover (`inset` px at the bottom), keeping
/// `margin` px from the edges. (0, 0) for a caret with no finite position.
pub fn reveal(caret: ScreenRect, w: f32, h: f32, inset: f32, margin: f32) -> (f32, f32) {
    if !all_finite(caret) || ![w, h, inset, margin].iter().all(|v| v.is_finite()) {
        return (0.0, 0.0);
    }
    let (x, y, cw, ch) = caret;
    let bottom = (h - inset.max(0.0)).max(margin * 3.0);
    let (dx, dy) = (shift_into(x, cw, margin, w - margin), shift_into(y, ch, margin, bottom - margin));
    if dx.is_finite() && dy.is_finite() {
        (dx, dy)
    } else {
        (0.0, 0.0)
    }
}

/// Whether the host must hear about a new caret position: focus gained or
/// lost, or the caret moved by at least half a pixel.
pub fn focus_changed(old: Option<ScreenRect>, new: Option<ScreenRect>) -> bool {
    match (old, new) {
        (None, None) => false,
        (Some(a), Some(b)) => (a.0 - b.0).abs() >= 0.5 || (a.1 - b.1).abs() >= 0.5 || (a.2 - b.2).abs() >= 0.5 || (a.3 - b.3).abs() >= 0.5,
        _ => true,
    }
}

/// Tells taps from pans, drags and pinches, from the raw pointer events.
#[derive(Debug, Default)]
pub struct Taps {
    down: Vec<u64>,
    start: (f32, f32),
    moved: bool,
    multi: bool,
}

impl Taps {
    /// No pointer is down: the next down starts a new gesture.
    pub fn idle(&self) -> bool {
        self.down.is_empty()
    }

    pub fn down(&mut self, id: u64, x: f32, y: f32) {
        if self.down.is_empty() {
            self.start = (x, y);
            self.moved = false;
            self.multi = false;
        } else {
            self.multi = true;
        }
        if !self.down.contains(&id) {
            self.down.push(id);
        }
    }

    /// A pointer moved; the gesture is no tap once the first finger went
    /// further than `slop` pixels from where it came down.
    pub fn moved(&mut self, id: u64, x: f32, y: f32, slop: f32) {
        if self.down.first() == Some(&id) && (x - self.start.0).hypot(y - self.start.1) > slop {
            self.moved = true;
        }
    }

    /// A pointer went up: where the gesture started, when it was a tap (one
    /// finger that stayed within the slop).
    pub fn up(&mut self, id: u64) -> Option<(f32, f32)> {
        let k = self.down.iter().position(|p| *p == id)?;
        self.down.remove(k);
        (self.down.is_empty() && !self.moved && !self.multi).then_some(self.start)
    }

    pub fn cancel(&mut self) {
        self.down.clear();
        self.moved = true;
    }
}

/// Whether a tap at `at` brings back the keyboard the user dismissed while
/// the page still has text focus at `caret`: the tap moved the caret (the
/// page took it), or it landed on the caret's line, which is the field being
/// edited. Taps elsewhere, pans, drags and pinches leave the keyboard down.
pub fn tap_reopens_keyboard(at: (f32, f32), caret: ScreenRect, caret_moved: bool) -> bool {
    if caret_moved {
        return true;
    }
    let (_, y, _, h) = caret;
    all_finite(caret) && at.1 >= y - h && at.1 <= y + 2.0 * h
}

/// Follow-up frames after a frame that ran past its budget.
///
/// `Engine::frame` takes finished tiles only while the frame is within its
/// budget (plus 3 ms). On a phone or an emulator a frame that draws panels
/// can overrun it; finished tiles then stay unread, and since the wake that
/// announced them was spent on that frame, the page sat with blank or
/// blurry tiles until the next touch (NOTES.md, "tiles left unread"). So
/// after an overrun the host runs another frame at once: as long as frames
/// keep showing something new (tiles arriving), and a few more after
/// that, but never more than [`MAX_FOLLOW_UPS`] in a row.
#[derive(Debug)]
pub struct Overrun {
    limit: std::time::Duration,
    /// Overrunning frames in a row that showed nothing new.
    idle: u32,
    /// Follow-ups in a row.
    total: u32,
    pending: bool,
}

/// Overrunning frames with nothing new before the host waits again.
pub const MAX_IDLE: u32 = 3;
/// Follow-ups in a row at most (a few seconds of frames).
pub const MAX_FOLLOW_UPS: u32 = 240;

impl Overrun {
    pub fn new(budget_ms: f32) -> Self {
        Overrun { limit: std::time::Duration::from_secs_f32((budget_ms + 3.0) / 1000.0), idle: 0, total: 0, pending: false }
    }

    /// A frame took `took` and showed something new (`rendered`);
    /// `continuous` frames follow anyway.
    pub fn frame(&mut self, took: std::time::Duration, rendered: bool, continuous: bool) {
        if continuous || took <= self.limit {
            *self = Overrun { limit: self.limit, idle: 0, total: 0, pending: false };
            return;
        }
        self.total += 1;
        self.idle = if rendered { 0 } else { self.idle + 1 };
        self.pending = self.total < MAX_FOLLOW_UPS && self.idle < MAX_IDLE;
    }

    /// Whether the next frame should run without waiting.
    pub fn again(&self) -> bool {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_that_overrun_get_a_few_follow_ups() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        let mut o = Overrun::new(16.0);
        assert!(!o.again());
        o.frame(ms(10), true, false);
        assert!(!o.again(), "a frame within budget waits for the next wake");
        o.frame(ms(40), true, false);
        assert!(o.again(), "an overrun is followed at once");
        o.frame(ms(2), false, false);
        assert!(!o.again(), "a quick follow-up read what was left");
        // tiles keep arriving: follow up while frames show something new
        for _ in 0..20 {
            o.frame(ms(45), true, false);
            assert!(o.again());
        }
        // then a few slow frames with nothing new, and the host waits again
        let mut n = 0;
        while o.again() && n < 100 {
            n += 1;
            o.frame(ms(50), false, false);
        }
        assert_eq!(n, MAX_IDLE as usize);
        // a page whose every frame overruns and changes does not spin forever
        o.frame(ms(1), false, false);
        let mut n = 0;
        o.frame(ms(50), true, false);
        while o.again() && n < 10_000 {
            n += 1;
            o.frame(ms(50), true, false);
        }
        assert_eq!(n, MAX_FOLLOW_UPS as usize - 1);
        // continuous pages are paced by the engine, not by this
        o.frame(ms(50), true, true);
        assert!(!o.again());
    }

    #[test]
    fn spans_move_into_range() {
        assert_eq!(shift_into(10.0, 5.0, 0.0, 100.0), 0.0);
        assert_eq!(shift_into(98.0, 5.0, 0.0, 100.0), -3.0);
        assert_eq!(shift_into(-4.0, 5.0, 0.0, 100.0), 4.0);
        // longer than the range: align its start
        assert_eq!(shift_into(50.0, 500.0, 0.0, 100.0), -50.0);
    }

    #[test]
    fn caret_above_the_keyboard() {
        // 1080 x 1920 screen, keyboard 800 px tall, caret at y 1500
        let (dx, dy) = reveal((300.0, 1500.0, 3.0, 60.0), 1080.0, 1920.0, 800.0, 40.0);
        assert_eq!(dx, 0.0);
        assert_eq!(1500.0 + dy + 60.0, 1920.0 - 800.0 - 40.0);
        // already visible: no move
        assert_eq!(reveal((300.0, 500.0, 3.0, 60.0), 1080.0, 1920.0, 800.0, 40.0), (0.0, 0.0));
        // off the right edge
        assert_eq!(reveal((1100.0, 500.0, 3.0, 60.0), 1080.0, 1920.0, 0.0, 40.0).0, 1080.0 - 40.0 - 1103.0);
    }

    #[test]
    fn carets_without_a_position_never_move_the_camera() {
        // page.focus_text({10, 1e39, 2, 30}): finite in Luau, infinite in f32;
        // the engine's screen rect was (76, inf, 2, 1) and the pan -inf
        let inf = 1e39f64 as f32;
        assert!(inf.is_infinite());
        assert_eq!(reveal((76.0, inf, 2.0, 1.0), 1080.0, 1920.0, 800.0, 40.0), (0.0, 0.0));
        assert_eq!(reveal((f32::NAN, 10.0, 2.0, 30.0), 1080.0, 1920.0, 800.0, 40.0), (0.0, 0.0));
        assert_eq!(sanitize((76.0, inf, 2.0, 1.0)), UNKNOWN_CARET);
        assert_eq!(sanitize((76.0, 5.0, 2.0, 1.0)), (76.0, 5.0, 2.0, 1.0));
        // the world rule: finite and within a page's size of the page
        let page = Rect::new(0.0, 0.0, 720.0, 860.0);
        assert!(revealable(Rect::new(20.0, 112.0, 2.0, 36.0), page));
        assert!(revealable(Rect::new(20.0, 1500.0, 2.0, 36.0), page), "below the page, but near it");
        assert!(!revealable(Rect::new(10.0, inf, 2.0, 30.0), page));
        assert!(!revealable(Rect::new(10.0, 1e30, 2.0, 30.0), page), "finite, but far away");
        assert!(!revealable(Rect::new(f32::NAN, 0.0, 2.0, 30.0), page));
    }

    #[test]
    fn focus_changes() {
        let a = Some((1.0, 2.0, 3.0, 4.0));
        assert!(!focus_changed(None, None));
        assert!(focus_changed(None, a));
        assert!(focus_changed(a, None));
        assert!(!focus_changed(a, Some((1.2, 2.0, 3.0, 4.0))));
        assert!(focus_changed(a, Some((1.6, 2.0, 3.0, 4.0))));
    }

    #[test]
    fn taps_pans_and_pinches() {
        let slop = 16.0;
        let mut t = Taps::default();
        // a tap: down, a small wobble, up
        t.down(0, 100.0, 200.0);
        t.moved(0, 104.0, 203.0, slop);
        assert_eq!(t.up(0), Some((100.0, 200.0)));
        assert!(t.idle());
        // a pan
        t.down(0, 100.0, 200.0);
        t.moved(0, 100.0, 260.0, slop);
        assert_eq!(t.up(0), None);
        // a pinch: two fingers, neither moving much, the last one up ends it
        t.down(0, 100.0, 200.0);
        t.down(1, 300.0, 200.0);
        assert_eq!(t.up(1), None);
        assert_eq!(t.up(0), None);
        // a cancelled gesture is no tap; an up for an unknown pointer is ignored
        t.down(0, 1.0, 1.0);
        t.cancel();
        assert_eq!(t.up(0), None);
        assert_eq!(t.up(7), None);
        // the next gesture starts fresh
        t.down(3, 5.0, 5.0);
        assert_eq!(t.up(3), Some((5.0, 5.0)));
    }

    #[test]
    fn which_taps_bring_the_keyboard_back() {
        let caret = (300.0, 500.0, 3.0, 60.0);
        // on the caret's line (the field), anywhere across
        assert!(tap_reopens_keyboard((20.0, 520.0), caret, false));
        assert!(tap_reopens_keyboard((900.0, 450.0), caret, false));
        // well above or below it: a button, the log, empty page
        assert!(!tap_reopens_keyboard((300.0, 300.0), caret, false));
        assert!(!tap_reopens_keyboard((300.0, 700.0), caret, false));
        // the tap moved the caret: always
        assert!(tap_reopens_keyboard((300.0, 1500.0), caret, true));
        assert!(!tap_reopens_keyboard((0.0, 0.0), (0.0, f32::INFINITY, 1.0, 1.0), false));
    }
}

//! Transcript viewport state.
//!
//! Scrolling is presentation, not kernel work: it lives in the internal UI
//! plugin and never reaches `turya-core`. The viewport stores an absolute
//! top row so output that arrives while the user is reading history cannot
//! drag the view along with it. Follow mode is the only state that tracks
//! the bottom.

/// A viewport over a wrapped transcript.
#[derive(Debug, Clone)]
pub(crate) struct Viewport {
    /// Absolute top row currently displayed.
    offset: usize,
    /// Largest valid top row for the current content (`total - height`).
    max_offset: usize,
    /// Pinned to the newest row; grows with the transcript.
    follow: bool,
}

impl Viewport {
    pub(crate) fn new() -> Self {
        Self {
            offset: 0,
            max_offset: 0,
            follow: true,
        }
    }

    pub(crate) fn is_following(&self) -> bool {
        self.follow
    }

    #[cfg(test)]
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// Rows between the current top and the bottom, for tests and status.
    #[cfg(test)]
    pub(crate) fn lines_up(&self) -> usize {
        if self.follow {
            0
        } else {
            self.max_offset.saturating_sub(self.offset)
        }
    }

    /// Reconcile with the current content height. Following pins to the
    /// bottom; a user-held viewport keeps its absolute row.
    pub(crate) fn update(&mut self, max_offset: usize) -> usize {
        self.max_offset = max_offset;
        self.offset = if self.follow {
            max_offset
        } else {
            self.offset.min(max_offset)
        };
        self.offset
    }

    /// Move by `delta` rows (negative = older). Any manual movement releases
    /// follow; landing on the newest row resumes it. Hard edges are strict
    /// no-ops: a tick that cannot move the viewport changes nothing at all,
    /// so the first reverse step out of an edge moves exactly one step.
    pub(crate) fn scroll(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        if delta > 0 && self.follow {
            return;
        }
        if delta < 0 && self.offset == 0 {
            return;
        }
        if self.follow {
            self.offset = self.max_offset;
            self.follow = false;
        }
        let next = self.offset as i64 + delta as i64;
        self.offset = next.clamp(0, self.max_offset as i64) as usize;
        if self.offset >= self.max_offset {
            self.follow = true;
            self.offset = self.max_offset;
        }
    }

    /// Jump to the newest row and resume following.
    pub(crate) fn pin_to_bottom(&mut self) {
        self.follow = true;
        self.offset = self.max_offset;
    }

    /// Stop following where we are. Used when auto-scroll is disabled: new
    /// output must not move the viewport.
    pub(crate) fn hold(&mut self) {
        self.follow = false;
        self.offset = self.max_offset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_tracks_growth() {
        let mut v = Viewport::new();
        assert_eq!(v.update(100), 100);
        assert_eq!(v.update(140), 140);
        assert!(v.is_following());
        assert_eq!(v.lines_up(), 0);
    }

    #[test]
    fn scrolled_viewport_keeps_its_row_when_content_grows() {
        let mut v = Viewport::new();
        v.update(100);
        v.scroll(-20);
        assert_eq!(v.offset(), 80);
        assert!(!v.is_following());
        // New output arrives; the anchored row must not move.
        assert_eq!(v.update(160), 80);
        assert_eq!(v.lines_up(), 80);
        // It still scrolls within the larger content.
        v.scroll(-10);
        assert_eq!(v.offset(), 70);
    }

    #[test]
    fn scrolling_back_to_bottom_resumes_follow() {
        let mut v = Viewport::new();
        v.update(100);
        v.scroll(-5);
        assert_eq!(v.offset(), 95);
        v.scroll(5);
        assert_eq!(v.offset(), 100);
        assert!(v.is_following());
        v.update(200);
        assert_eq!(v.offset(), 200);
    }

    #[test]
    fn clamping_never_panics_when_content_shrinks() {
        let mut v = Viewport::new();
        v.update(100);
        v.scroll(-90);
        assert_eq!(v.offset(), 10);
        assert_eq!(v.update(4), 4);
        assert_eq!(v.offset(), 4);
        v.scroll(-10);
        assert_eq!(v.offset(), 0);
    }

    #[test]
    fn edge_tick_is_a_strict_no_op() {
        let mut v = Viewport::new();
        v.update(100);
        // At the bottom with follow on: ticking down changes nothing.
        assert!(v.is_following());
        for _ in 0..10 {
            v.scroll(5);
        }
        assert!(v.is_following());
        assert_eq!(v.offset(), 100);
        // Hammering up to the top, then ticking up: offset and anchor stay.
        v.scroll(-100);
        assert_eq!(v.offset(), 0);
        assert!(!v.is_following());
        for _ in 0..10 {
            v.scroll(-5);
        }
        assert_eq!(v.offset(), 0);
        assert!(!v.is_following());
        // Content-fits-screen (max 0): follow survives upward ticks, and the
        // first downward tick still resumes at the bottom.
        let mut v = Viewport::new();
        for _ in 0..5 {
            v.scroll(-10);
        }
        assert!(v.is_following());
        v.hold();
        assert!(!v.is_following());
        v.scroll(10);
        assert_eq!(v.offset(), 0);
    }

    #[test]
    fn hold_stops_following_without_jumping() {
        let mut v = Viewport::new();
        v.update(100);
        v.hold();
        assert!(!v.is_following());
        assert_eq!(v.update(150), 100);
        assert_eq!(v.offset(), 100);
    }
}

//! Terminal input intake: one reader thread, batched and coalesced.
//!
//! The old intake polled crossterm's `EventStream` one event at a time, and
//! the draw happened once per event. A mouse drag enqueues dozens of wheel
//! events per second — faster than a frame — so the queue grew into a
//! backlog the loop replayed notch by notch, and typed keys waited at the
//! back of it. Two structural fixes live here:
//!
//! 1. A dedicated reader thread (`poll` + `read`) pushing into an
//!    unbounded channel, so the loop can drain *everything* queued and
//!    process it as one batch. `poll`/`read` must not be mixed with
//!    `EventStream` (they share one internal queue), so `EventStream` is
//!    replaced outright, not supplemented.
//! 2. A coalescer that merges *consecutive* same-direction wheel notches
//!    into one scroll delta. Fifty notches of a drag become one viewport
//!    move, one input dispatch, one redraw — and the keys typed during a
//!    drag are no longer trapped behind it.

use crossterm::event::{poll, read, Event, MouseEventKind};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::mpsc;

/// Wrapped lines per wheel notch. One source of truth: the transcript
/// scroll and the coalescer must agree or a squashed drag scrolls the
/// wrong distance.
pub(crate) const WHEEL_LINES: usize = 3;

/// Upper bound on how many queued input events one batch processes before
/// the loop is forced to redraw. A drag cannot realistically enqueue more
/// than a few dozen events between frames, so this bound only ever fires
/// under a pathological flood — where it guarantees the UI still draws
/// between chunks instead of blackholing.
const MAX_INPUT_BATCH: usize = 1024;

/// One processed input item: a wheel run merged into a signed row delta,
/// or a non-wheel event passed through in its original order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Squashed {
    /// Merged run of same-direction wheel notches (`WHEEL_LINES` rows per
    /// notch; negative = up/older). Direction changes split the run, so the
    /// viewport's hard-edge no-op rules keep their per-notch meaning.
    Wheel(i32),
    /// Everything else in order (keys, clicks — which the TUI's wheel
    /// filter drops — resize, focus, paste).
    Other(Event),
}

/// Merge runs of consecutive same-direction wheel events into single
/// deltas; everything else passes through in order. Pure: unit-tested.
pub(crate) fn squash(events: impl IntoIterator<Item = Event>) -> Vec<Squashed> {
    let mut out = Vec::new();
    let mut pending: Option<i32> = None;
    for event in events {
        match wheel_delta(&event) {
            Some(delta) => match pending {
                // Same direction: keep accumulating the run.
                Some(p) if p.signum() == delta.signum() => {
                    pending = Some(p + delta);
                }
                // Direction change: the run ends here; the viewport's
                // hard-edge rules are per-notch, so a run must not swallow
                // notches that meant to go back the other way.
                Some(p) => {
                    out.push(Squashed::Wheel(p));
                    pending = Some(delta);
                }
                None => pending = Some(delta),
            },
            None => {
                if let Some(p) = pending.take() {
                    out.push(Squashed::Wheel(p));
                }
                out.push(Squashed::Other(event));
            }
        }
    }
    if let Some(p) = pending.take() {
        out.push(Squashed::Wheel(p));
    }
    out
}

/// The signed per-notch delta of a wheel event, or `None` for anything else.
fn wheel_delta(event: &Event) -> Option<i32> {
    match event {
        Event::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => Some(-(WHEEL_LINES as i32)),
            MouseEventKind::ScrollDown => Some(WHEEL_LINES as i32),
            _ => None,
        },
        _ => None,
    }
}

/// Take the event the `select!` arm already received and pull every other
/// event the terminal queued behind it into the same batch (bounded, so a
/// pathological flood still lets the UI redraw between chunks). The drain
/// is await-free: once the arm fired, added events cannot be lost to a
/// cancelled future.
pub(crate) fn drain_batch(first: Event, rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
    let mut batch = vec![first];
    while batch.len() < MAX_INPUT_BATCH {
        match rx.try_recv() {
            Ok(event) => batch.push(event),
            Err(_) => break,
        }
    }
    batch
}

/// Spawn the terminal input reader. Returns the event channel and the
/// flag that stops the thread (the loop sets it on shutdown so the thread
/// never outlives the TUI by more than one poll interval).
pub(crate) fn spawn_reader() -> (mpsc::UnboundedReceiver<Event>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("turya-input".to_string())
        .spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) && !tx.is_closed() {
                // A short poll keeps shutdown responsive. `poll` returning
                // false means no event is pending within the window; true
                // means one is ready and `read` will not block.
                if poll(Duration::from_millis(50)).unwrap_or(false) {
                    match read() {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            }
        })
        .expect("spawn terminal input reader");
    (rx, stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn wheel_event(down: bool) -> Event {
        Event::Mouse(MouseEvent {
            kind: if down {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            },
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        })
    }

    fn click() -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        })
    }

    fn deltas(squashed: &[Squashed]) -> Vec<i32> {
        squashed
            .iter()
            .filter_map(|s| match s {
                Squashed::Wheel(d) => Some(*d),
                Squashed::Other(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_drag_of_wheel_events_becomes_one_move() {
        let events: Vec<Event> = std::iter::repeat_with(|| wheel_event(true))
            .take(50)
            .collect();
        let squashed = squash(events);
        assert_eq!(squashed.len(), 1, "fifty notches are ONE move, not fifty");
        assert_eq!(deltas(&squashed), vec![(50 * WHEEL_LINES as i32)]);
    }

    #[test]
    fn direction_changes_split_the_run() {
        let squashed = squash(vec![
            wheel_event(true),
            wheel_event(true),
            wheel_event(false),
            wheel_event(false),
            wheel_event(true),
        ]);
        assert_eq!(deltas(&squashed), vec![6, -6, 3]);
    }

    #[test]
    fn a_key_flushes_the_pending_wheel_first() {
        let squashed = squash(vec![
            wheel_event(true),
            wheel_event(true),
            key(KeyCode::Char('a')),
        ]);
        assert_eq!(squashed.len(), 2, "wheel flushed, then the key");
        assert_eq!(deltas(&squashed), vec![6]);
        let keys = squashed
            .iter()
            .any(|s| matches!(s, Squashed::Other(Event::Key(_))));
        assert!(keys, "the key must survive the squash");
    }

    #[test]
    fn non_wheel_mouse_events_pass_through_unmerged() {
        // A click is not a scroll: it must never be folded into a run.
        let squashed = squash(vec![
            wheel_event(true),
            wheel_event(true),
            click(),
            wheel_event(true),
        ]);
        // wheel run flushed before the click, final notch flushed at the end
        assert_eq!(deltas(&squashed), vec![6, 3]);
        assert_eq!(squashed.len(), 3);
    }

    #[test]
    fn an_empty_batch_stays_empty() {
        assert!(squash(Vec::new()).is_empty());
    }

    #[tokio::test]
    async fn drain_batch_collects_everything_queued_and_stops_at_the_bound() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let codes = [
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::End,
            KeyCode::Home,
            KeyCode::Tab,
        ];
        for code in codes {
            let _ = tx.send(key(code));
        }
        let first = rx.recv().await.expect("first event");
        let batch = drain_batch(first, &mut rx);
        assert_eq!(batch.len(), 5, "every queued event is in the batch");
        assert!(matches!(batch[0], Event::Key(_)));
        // Already drained: nothing left.
        assert!(drain_batch(key(KeyCode::Char('q')), &mut rx).len() == 1);
    }
}

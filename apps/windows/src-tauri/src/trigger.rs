//! The gesture thread: turns timestamped chord signals from the keyboard
//! hook into engine actions, and settles the double-tap window on time.
//!
//! The hook callback must stay O(1), so it only `try_send`s onto a bounded
//! channel; every timing decision happens here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Instant;

use tokio::sync::mpsc::UnboundedSender;

use crate::chord::ChordSignal;
use crate::gesture::{Action, GestureMachine};

/// Deep enough that a burst of key events never fills it.
const QUEUE: usize = 256;

pub enum Input {
    Chord(ChordSignal, Instant),
    /// The engine finished or refused this Recording.
    Ended(u64),
}

/// The hook's end of the gesture thread.
#[derive(Clone)]
pub struct Trigger {
    tx: SyncSender<Input>,
    armed: Arc<AtomicBool>,
}

impl Trigger {
    /// Whether a Recording or pending tap is live: Esc then cancels.
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }

    /// Never blocks, allocates or logs, so the hook callback may call it. A
    /// full queue drops the signal.
    pub fn signal(&self, signal: ChordSignal) {
        self.send(Input::Chord(signal, Instant::now()));
    }

    pub fn ended(&self, id: u64) {
        self.send(Input::Ended(id));
    }

    fn send(&self, input: Input) {
        let _ = self.tx.try_send(input);
    }
}

pub fn spawn(engine: UnboundedSender<Action>) -> std::io::Result<Trigger> {
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let armed = Arc::new(AtomicBool::new(false));
    let thread_armed = Arc::clone(&armed);
    thread::Builder::new()
        .name("voisu-gesture".into())
        .spawn(move || {
            let mut machine = GestureMachine::default();
            loop {
                let input = match machine.next_deadline() {
                    Some(deadline) => {
                        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    }
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                let actions = match input {
                    Ok(Input::Chord(signal, at)) => machine.on_signal(signal, at),
                    Ok(Input::Ended(id)) => {
                        machine.on_engine_ended(id);
                        Vec::new()
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        machine.on_timeout(Instant::now()).into_iter().collect()
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                thread_armed.store(machine.armed(), Ordering::Release);
                for action in actions {
                    tracing::debug!(?action, "trigger");
                    if engine.send(action).is_err() {
                        return;
                    }
                }
            }
        })?;
    Ok(Trigger { tx, armed })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::gesture::{DOUBLE_TAP, MIN_HOLD};

    fn recv(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Action>) -> Action {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(action) = rx.try_recv() {
                return action;
            }
            assert!(Instant::now() < deadline, "no action");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn lapsed_tap_is_cancelled_without_further_input() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let trigger = spawn(tx).unwrap();
        trigger.signal(ChordSignal::Pressed);
        assert_eq!(recv(&mut rx), Action::Start(1));
        trigger.signal(ChordSignal::Released);
        // The thread wakes on its own once the double-tap window lapses.
        thread::sleep(DOUBLE_TAP + Duration::from_millis(50));
        assert_eq!(recv(&mut rx), Action::Cancel(1));
        assert!(!trigger.armed());
    }

    #[test]
    fn hold_then_release_stops() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let trigger = spawn(tx).unwrap();
        trigger.signal(ChordSignal::Pressed);
        assert_eq!(recv(&mut rx), Action::Start(1));
        thread::sleep(MIN_HOLD + Duration::from_millis(20));
        trigger.signal(ChordSignal::Released);
        assert_eq!(recv(&mut rx), Action::Stop(1));
    }

    #[test]
    fn engine_end_disarms() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let trigger = spawn(tx).unwrap();
        trigger.signal(ChordSignal::Pressed);
        assert_eq!(recv(&mut rx), Action::Start(1));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !trigger.armed() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        trigger.ended(1);
        while trigger.armed() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
}

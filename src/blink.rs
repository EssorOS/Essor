use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::Duration;

/// Caret blink half-period.
pub const BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// A periodic timer whose wait is restarted whenever [`BlinkTimer::reset`] is
/// called. On every interval of inactivity it invokes the `wake` callback; the
/// timer stops when `wake` returns `false`.
pub struct BlinkTimer {
    reset: Option<Sender<()>>,
}

impl BlinkTimer {
    /// A timer that never fires. Used when no event loop is available (tests).
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self { reset: None }
    }

    /// Spawn the timer thread. `wake` runs on that thread and should return
    /// `false` to stop the timer (e.g. when the event loop has shut down).
    pub fn spawn<F>(mut wake: F) -> Self
    where
        F: FnMut() -> bool + Send + 'static,
    {
        let (reset, reset_rx) = channel::<()>();
        std::thread::spawn(move || {
            loop {
                match reset_rx.recv_timeout(BLINK_INTERVAL) {
                    Ok(()) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        if !wake() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { reset: Some(reset) }
    }

    /// Restart the interval so the next tick is a full period away.
    pub fn reset(&self) {
        if let Some(reset) = &self.reset {
            let _ = reset.send(());
        }
    }
}

//! App lifecycle: which callbacks a state change runs.
//!
//! ```text
//!             on_pause                       (work done / over budget)
//! Foreground ─────────► Background ────────────────────► Suspended
//!     ▲  ▲                   │                              │
//!     │  └─── on_resume ─────┘                              │
//!     └────────────────────── on_resume ────────────────────┘
//!
//! any running state ── on_stop ──► Stopped (destroyed)
//! ```
//!
//! An app leaving the foreground becomes `Background` when it is granted a
//! background capability (declared in its `AppInfo` *and* requested by
//! `App::background_work`), otherwise `Suspended`. A suspended app gets no
//! CPU, so it cannot ask for background work again; only the user brings
//! it back.

use p4os_sdk::{AppState, Capabilities};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Callback {
    Pause,
    Resume,
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InvalidTransition {
    pub from: AppState,
    pub to: AppState,
}

/// Callbacks to run, in order, when an app moves from `from` to `to`.
pub fn transition(from: AppState, to: AppState) -> Result<&'static [Callback], InvalidTransition> {
    use AppState::*;
    match (from, to) {
        (a, b) if a == b && a != Stopped => Ok(&[]),
        (Foreground, Background | Suspended) => Ok(&[Callback::Pause]),
        (Background | Suspended, Foreground) => Ok(&[Callback::Resume]),
        (Background, Suspended) => Ok(&[]),
        (Foreground, Stopped) => Ok(&[Callback::Pause, Callback::Stop]),
        (Background | Suspended, Stopped) => Ok(&[Callback::Stop]),
        _ => Err(InvalidTransition { from, to }),
    }
}

/// Capabilities an app gets: what it asks for, limited to what it declared.
pub fn granted(declared: Capabilities, requested: Capabilities) -> Capabilities {
    declared.intersect(requested)
}

/// State of an app that has just left the foreground or is re-evaluated
/// while in the background.
pub fn off_screen_state(granted: Capabilities) -> AppState {
    if granted.is_empty() { AppState::Suspended } else { AppState::Background }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AppState::*;

    #[test]
    fn leaving_and_returning() {
        assert_eq!(transition(Foreground, Background), Ok(&[Callback::Pause][..]));
        assert_eq!(transition(Foreground, Suspended), Ok(&[Callback::Pause][..]));
        assert_eq!(transition(Background, Foreground), Ok(&[Callback::Resume][..]));
        assert_eq!(transition(Suspended, Foreground), Ok(&[Callback::Resume][..]));
        assert_eq!(transition(Background, Suspended), Ok(&[][..]));
    }

    #[test]
    fn stopping_pauses_a_foreground_app_first() {
        assert_eq!(transition(Foreground, Stopped), Ok(&[Callback::Pause, Callback::Stop][..]));
        assert_eq!(transition(Background, Stopped), Ok(&[Callback::Stop][..]));
        assert_eq!(transition(Suspended, Stopped), Ok(&[Callback::Stop][..]));
    }

    #[test]
    fn invalid_transitions() {
        // A suspended app has no CPU to ask for background work with.
        assert!(transition(Suspended, Background).is_err());
        // Nothing comes back from the dead, and it cannot die twice.
        assert!(transition(Stopped, Foreground).is_err());
        assert!(transition(Stopped, Stopped).is_err());
        assert_eq!(transition(Background, Background), Ok(&[][..]));
    }

    #[test]
    fn capabilities_must_be_declared() {
        let declared = Capabilities::AUDIO;
        assert_eq!(granted(declared, Capabilities::AUDIO), Capabilities::AUDIO);
        assert!(granted(declared, Capabilities::NETWORK).is_empty());
        assert_eq!(granted(declared, Capabilities::AUDIO.union(Capabilities::NETWORK)), Capabilities::AUDIO);
        assert_eq!(off_screen_state(Capabilities::NONE), Suspended);
        assert_eq!(off_screen_state(Capabilities::AUDIO), Background);
    }
}

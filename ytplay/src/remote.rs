//! Hand-offs between the console (`yt` shell commands, one thread) and the
//! player (GUI thread): a small command queue, the latest statistics line,
//! the shared visitorData, and fetches orphaned by a closed player.
//!
//! Everything is lock-free (atomics only), so the shell and the GUI thread
//! never wait on each other.

use crate::innertube::Quality;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use p4os_sdk::FetchId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Video(Quality),
    Audio,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Play { video_id: String, mode: Mode },
    /// Relative seek in seconds.
    Seek(i32),
    TogglePause,
    Stop,
}

pub const COMMAND_USAGE: &str = "play ID|URL [144|240|audio] | pause | seek [+-]SECONDS | stop";

/// Parse the arguments of a `yt` console command.
pub fn parse_command(args: &str) -> Result<Command, &'static str> {
    let mut it = args.split_whitespace();
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some("play"), Some(id), mode, None) => {
            let mode = match mode {
                None | Some("144") => Mode::Video(Quality::P144),
                Some("240") => Mode::Video(Quality::P240),
                Some("audio") => Mode::Audio,
                Some(_) => return Err("mode must be 144, 240 or audio"),
            };
            let video_id = crate::player::parse_video_id(id).ok_or("not a YouTube video id or link")?;
            Ok(Command::Play { video_id, mode })
        }
        (Some("pause"), None, ..) => Ok(Command::TogglePause),
        (Some("stop"), None, ..) => Ok(Command::Stop),
        (Some("seek"), Some(s), None, None) => s.trim_start_matches('+').parse().map(Command::Seek).map_err(|_| "seek takes whole seconds"),
        _ => Err(COMMAND_USAGE),
    }
}

const SLOTS: usize = 8;
static QUEUE: [AtomicPtr<Command>; SLOTS] = [const { AtomicPtr::new(ptr::null_mut()) }; SLOTS];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

/// Queue a command for the player. Call from one thread only (the shell).
/// Returns false when the queue is full.
pub fn post(cmd: Command) -> bool {
    let tail = TAIL.load(Ordering::Relaxed);
    let slot = &QUEUE[tail % SLOTS];
    if !slot.load(Ordering::Acquire).is_null() {
        return false;
    }
    slot.store(Box::into_raw(Box::new(cmd)), Ordering::Release);
    TAIL.store(tail.wrapping_add(1), Ordering::Release);
    true
}

/// Take the next command. Call from one thread only (the GUI).
pub fn take() -> Option<Command> {
    let head = HEAD.load(Ordering::Relaxed);
    if head == TAIL.load(Ordering::Acquire) {
        return None;
    }
    let p = QUEUE[head % SLOTS].swap(ptr::null_mut(), Ordering::AcqRel);
    HEAD.store(head.wrapping_add(1), Ordering::Release);
    // SAFETY: non-null slots hold a pointer from `Box::into_raw` in `post`.
    (!p.is_null()).then(|| *unsafe { Box::from_raw(p) })
}

/// True when a command waits: the desktop opens the player for it.
pub fn pending() -> bool {
    HEAD.load(Ordering::Acquire) != TAIL.load(Ordering::Acquire)
}

/// A string slot that one side replaces and the other reads.
struct Shared(AtomicPtr<String>);

impl Shared {
    const fn new() -> Self {
        Self(AtomicPtr::new(ptr::null_mut()))
    }

    fn set(&self, s: String) {
        let old = self.0.swap(Box::into_raw(Box::new(s)), Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: every stored pointer comes from `Box::into_raw`, and the
            // swap made this side its only owner.
            drop(unsafe { Box::from_raw(old) });
        }
    }

    fn get(&self) -> Option<String> {
        let p = self.0.swap(ptr::null_mut(), Ordering::AcqRel);
        if p.is_null() {
            return None;
        }
        // SAFETY: as in `set`; the swap took ownership.
        let b = unsafe { Box::from_raw(p) };
        let copy = (*b).clone();
        // Put it back unless a newer value arrived meanwhile.
        let raw = Box::into_raw(b);
        if self.0.compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire).is_err() {
            // SAFETY: the exchange failed, so `raw` is still ours alone.
            drop(unsafe { Box::from_raw(raw) });
        }
        Some(copy)
    }
}

static STATS: Shared = Shared::new();
static VISITOR: Shared = Shared::new();

/// Latest one-line player statistics, for `yt stat`.
pub fn publish_stats(line: String) {
    STATS.set(line);
}

pub fn stats() -> Option<String> {
    STATS.get()
}

/// visitorData handed out by YouTube, reused by later lookups.
pub fn set_visitor_data(v: String) {
    VISITOR.set(v);
}

pub fn visitor_data() -> Option<String> {
    VISITOR.get()
}

const ORPHANS: usize = 8;
static ORPHAN: [AtomicUsize; ORPHANS] = [const { AtomicUsize::new(0) }; ORPHANS];

/// Remember a fetch whose player was closed: its result must still be
/// collected or the network layer keeps the body. Ids are never 0.
pub fn orphan(id: FetchId) {
    for slot in &ORPHAN {
        if slot.compare_exchange(0, id as usize, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return;
        }
    }
}

/// Take every orphaned fetch id.
pub fn take_orphans() -> Vec<FetchId> {
    ORPHAN.iter().map(|s| s.swap(0, Ordering::AcqRel) as FetchId).filter(|&id| id != 0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_console_commands() {
        let id = String::from("dQw4w9WgXcQ");
        assert_eq!(parse_command("play dQw4w9WgXcQ"), Ok(Command::Play { video_id: id.clone(), mode: Mode::Video(Quality::P144) }));
        assert_eq!(parse_command("play https://youtu.be/dQw4w9WgXcQ audio"), Ok(Command::Play { video_id: id.clone(), mode: Mode::Audio }));
        assert_eq!(parse_command("play dQw4w9WgXcQ 240"), Ok(Command::Play { video_id: id, mode: Mode::Video(Quality::P240) }));
        assert_eq!(parse_command("seek -10"), Ok(Command::Seek(-10)));
        assert_eq!(parse_command("seek +30"), Ok(Command::Seek(30)));
        assert_eq!(parse_command("pause"), Ok(Command::TogglePause));
        assert!(parse_command("play nope").is_err());
        assert!(parse_command("play dQw4w9WgXcQ 1080").is_err());
        assert!(parse_command("seek x").is_err());
        assert!(parse_command("").is_err());
    }

    // One test touches the global queue, so tests cannot race on it.
    #[test]
    fn queues_commands_and_shares_strings() {
        assert!(take().is_none());
        assert!(post(Command::Seek(10)));
        assert!(post(Command::TogglePause));
        assert!(pending());
        assert_eq!(take(), Some(Command::Seek(10)));
        assert_eq!(take(), Some(Command::TogglePause));
        assert!(!pending());
        for _ in 0..SLOTS {
            assert!(post(Command::Stop));
        }
        assert!(!post(Command::Stop));
        while take().is_some() {}

        assert_eq!(stats(), None);
        publish_stats("a".into());
        publish_stats("b".into());
        assert_eq!(stats().as_deref(), Some("b"));
        assert_eq!(stats().as_deref(), Some("b"));

        orphan(5);
        orphan(7);
        let mut o = take_orphans();
        o.sort();
        assert_eq!(o, [5, 7]);
        assert!(take_orphans().is_empty());
    }
}

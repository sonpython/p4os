//! The subscriptions feed: recent uploads of subscribed channels.
//!
//! The Data API has no "subscriptions feed" call, so it is assembled:
//!
//! 1. `subscriptions.list` (mine, by relevance): up to [`CHANNELS`] channels, 1 unit;
//! 2. `channels.list` for their uploads playlists, 1 unit;
//! 3. `playlistItems.list` per channel, newest [`PER_CHANNEL`] uploads, 1 unit each;
//! 4. `videos.list` for durations and view counts, 1 unit.
//!
//! About 23 units per refresh with 20 channels, so the app caches the
//! result for [`CACHE_MS`]. (`search.list` with `channelId` would cost 100
//! units per channel.)

use super::api::{self, Call, Video};
use alloc::string::String;
use alloc::vec::Vec;

pub const CHANNELS: u32 = 20;
pub const PER_CHANNEL: u32 = 3;
pub const SIZE: usize = 30;
pub const CACHE_MS: u64 = 15 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Subscriptions,
    Channels,
    Uploads,
    Stats,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    /// Send these calls and report each answer with its step.
    Calls(Vec<(Call, Step)>),
    /// Waiting for more answers.
    Wait,
    Done(Vec<Video>),
    Failed(String),
}

#[derive(Default)]
pub struct Builder {
    uploads_pending: usize,
    videos: Vec<Video>,
}

impl Builder {
    pub fn start() -> (Self, (Call, Step)) {
        (Self::default(), (api::subscriptions(CHANNELS, None), Step::Subscriptions))
    }

    pub fn on_response(&mut self, step: Step, result: Result<Vec<u8>, String>) -> Progress {
        match step {
            Step::Subscriptions => {
                let subs = match result.and_then(|b| api::parse_subscriptions(&b)) {
                    Ok(p) => p.items,
                    Err(e) => return Progress::Failed(e),
                };
                if subs.is_empty() {
                    return Progress::Done(Vec::new());
                }
                let ids: Vec<&str> = subs.iter().map(|c| c.id.as_str()).collect();
                Progress::Calls(alloc::vec![(api::uploads(&ids), Step::Channels)])
            }
            Step::Channels => {
                let uploads = match result.and_then(|b| api::parse_uploads(&b)) {
                    Ok(u) => u,
                    Err(e) => return Progress::Failed(e),
                };
                if uploads.is_empty() {
                    return Progress::Done(Vec::new());
                }
                self.uploads_pending = uploads.len();
                Progress::Calls(
                    uploads.iter().map(|(_, pl)| (api::playlist_items(pl, PER_CHANNEL, None), Step::Uploads)).collect(),
                )
            }
            Step::Uploads => {
                // One channel failing (deleted, private uploads) does not
                // spoil the feed.
                if let Ok(page) = result.and_then(|b| api::parse_playlist_items(&b)) {
                    self.videos.extend(page.items);
                }
                self.uploads_pending = self.uploads_pending.saturating_sub(1);
                if self.uploads_pending > 0 {
                    return Progress::Wait;
                }
                self.videos = api::newest(core::mem::take(&mut self.videos), SIZE);
                if self.videos.is_empty() {
                    return Progress::Done(Vec::new());
                }
                let ids: Vec<&str> = self.videos.iter().map(|v| v.id.as_str()).collect();
                Progress::Calls(alloc::vec![(api::stats(&ids), Step::Stats)])
            }
            Step::Stats => {
                // Without stats the feed still works, just without durations.
                if let Ok(page) = result.and_then(|b| api::parse_videos(&b)) {
                    api::merge_stats(&mut self.videos, &page.items);
                }
                Progress::Done(core::mem::take(&mut self.videos))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Result<Vec<u8>, String> {
        Ok(s.as_bytes().to_vec())
    }

    #[test]
    fn builds_the_feed_from_fixtures() {
        let (mut b, (first, step)) = Builder::start();
        assert!(first.path.starts_with("subscriptions?") && first.path.contains("mine=true"));
        let mut cost = first.cost;
        let Progress::Calls(calls) = b.on_response(step, ok(include_str!("fixtures/subscriptions.json"))) else { panic!() };
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0.path.contains("id=UCp4osChannelAAAAAAAAAAA,UCp4osChannelCCCCCCCCCCC,UCp4osChannelGGGGGGGGGGG"));
        cost += calls[0].0.cost;
        let Progress::Calls(uploads) = b.on_response(Step::Channels, ok(include_str!("fixtures/channels.json"))) else { panic!() };
        assert_eq!(uploads.len(), 3);
        assert!(uploads[1].0.path.contains("playlistId=UUp4osChannelCCCCCCCCCCC"));
        cost += uploads.iter().map(|c| c.0.cost).sum::<u32>();
        assert_eq!(b.on_response(Step::Uploads, ok(include_str!("fixtures/uploads-1.json"))), Progress::Wait);
        // A failing channel is skipped.
        assert_eq!(b.on_response(Step::Uploads, Err("HTTP 404".into())), Progress::Wait);
        let Progress::Calls(stats) = b.on_response(Step::Uploads, ok(include_str!("fixtures/uploads-3.json"))) else { panic!() };
        cost += stats[0].0.cost;
        let Progress::Done(feed) = b.on_response(Step::Stats, ok(include_str!("fixtures/videos.json"))) else { panic!() };
        let ids: Vec<&str> = feed.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["p4Os_Feed01", "p4Os_Feed06", "p4Os_Feed02", "p4Os_Feed03", "p4Os_Feed07"]);
        assert_eq!(feed[0].duration_s, Some(21 * 60 + 7));
        assert_eq!(feed[0].views, Some(88_123));
        assert_eq!(cost, 1 + 1 + 3 + 1, "quota units for one refresh");
    }

    #[test]
    fn empty_and_failed_feeds() {
        let (mut b, _) = Builder::start();
        assert_eq!(b.on_response(Step::Subscriptions, ok(r#"{"items":[]}"#)), Progress::Done(Vec::new()));
        let (mut b, _) = Builder::start();
        assert_eq!(b.on_response(Step::Subscriptions, Err("Sign in to see this.".into())), Progress::Failed("Sign in to see this.".into()));
        let (mut b, _) = Builder::start();
        b.on_response(Step::Subscriptions, ok(include_str!("fixtures/subscriptions.json")));
        b.on_response(Step::Channels, ok(include_str!("fixtures/channels.json")));
        for _ in 0..2 {
            assert_eq!(b.on_response(Step::Uploads, Err("x".into())), Progress::Wait);
        }
        assert_eq!(b.on_response(Step::Uploads, Err("x".into())), Progress::Done(Vec::new()));
    }
}

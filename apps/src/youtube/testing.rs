//! Test doubles: a scripted [`Network`] that records requests and answers
//! them with canned responses, and a [`System`] around it.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::{
    Audio, Color, DateTime, FetchId, FetchState, FileSystem, HttpRequest, Image, Network, System, SystemStats,
};

#[derive(Default)]
pub struct MockNet {
    pub requests: Vec<HttpRequest>,
    /// Requests not yet collected by a final `poll`.
    states: BTreeMap<FetchId, FetchState>,
    pub cancelled: usize,
}

impl MockNet {
    pub fn sent(&self) -> usize {
        self.requests.len()
    }

    pub fn last_request(&self) -> &HttpRequest {
        self.requests.last().expect("no request sent")
    }

    fn last_id(&self) -> FetchId {
        self.requests.len() as FetchId
    }

    pub fn respond_last(&mut self, status: u16, body: &str) {
        let id = self.last_id();
        self.respond(id, status, body);
    }

    pub fn respond(&mut self, id: FetchId, status: u16, body: &str) {
        let done = FetchState::Done { status, content_type: "application/json".into(), body: body.as_bytes().to_vec() };
        self.states.insert(id, done);
    }

    pub fn fail_last(&mut self, msg: &str) {
        let id = self.last_id();
        self.states.insert(id, FetchState::Failed(msg.into()));
    }

    /// Answer every pending request whose URL contains `pattern`; returns
    /// how many were answered.
    pub fn respond_matching(&mut self, pattern: &str, status: u16, body: &str) -> usize {
        let ids: Vec<FetchId> = self
            .states
            .iter()
            .filter(|(id, s)| matches!(s, FetchState::Pending) && self.requests[**id as usize - 1].url.contains(pattern))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.respond(*id, status, body);
        }
        ids.len()
    }

    /// URLs of requests still waiting for an answer.
    pub fn pending_urls(&self) -> Vec<String> {
        self.states
            .iter()
            .filter(|(_, s)| matches!(s, FetchState::Pending))
            .map(|(id, _)| self.requests[*id as usize - 1].url.clone())
            .collect()
    }

    /// Requests whose final state has not been collected.
    pub fn unpolled(&self) -> usize {
        self.states.len()
    }
}

impl Network for MockNet {
    fn status(&self) -> String {
        "mock".into()
    }

    fn is_up(&self) -> bool {
        true
    }

    fn fetch(&mut self, url: &str) -> FetchId {
        self.send(HttpRequest::get(url))
    }

    fn send(&mut self, req: HttpRequest) -> FetchId {
        self.requests.push(req);
        let id = self.last_id();
        self.states.insert(id, FetchState::Pending);
        id
    }

    fn cancel(&mut self, id: FetchId) {
        self.states.remove(&id);
        self.cancelled += 1;
    }

    fn poll(&mut self, id: FetchId) -> FetchState {
        match self.states.get(&id) {
            Some(FetchState::Pending) => FetchState::Pending,
            Some(_) => self.states.remove(&id).expect("present"),
            None => FetchState::Failed("unknown request".into()),
        }
    }
}

/// Just enough `System` for the app: the mock network, an in-memory
/// settings store, a settable clock and a decoder that turns any
/// non-empty data into a 1x1 image.
#[derive(Default)]
pub struct TestSys {
    pub net: MockNet,
    pub config: BTreeMap<String, String>,
    pub uptime: u64,
    pub decoded: usize,
}

impl System for TestSys {
    fn uptime_ms(&self) -> u64 {
        self.uptime
    }
    fn now(&self) -> Option<DateTime> {
        Some(DateTime { year: 2026, month: 10, day: 28, hour: 14, minute: 0, second: 0 })
    }
    fn set_time(&mut self, _t: DateTime) {}
    fn fs(&mut self) -> &mut dyn FileSystem {
        unimplemented!("the YouTube app does not use files")
    }
    fn audio(&mut self) -> &mut dyn Audio {
        unimplemented!("the YouTube app does not use audio")
    }
    fn net(&mut self) -> &mut dyn Network {
        &mut self.net
    }
    fn stats(&self) -> SystemStats {
        SystemStats::default()
    }
    fn decode_image(&mut self, data: &[u8], _w: i32, _h: i32) -> Option<Image> {
        self.decoded += 1;
        (!data.is_empty()).then(|| Image { width: 1, height: 1, pixels: alloc::vec![Color::WHITE] })
    }
    fn brightness(&self) -> f32 {
        1.0
    }
    fn set_brightness(&mut self, _v: f32) {}
    fn run_command(&mut self, _l: &str) -> String {
        String::new()
    }
    fn launch(&mut self, _id: &str) {}
    fn config_get(&self, key: &str) -> Option<String> {
        self.config.get(key).cloned()
    }
    fn config_set(&mut self, key: &str, value: &str) {
        self.config.insert(key.into(), value.into());
    }
    fn config_remove(&mut self, key: &str) {
        self.config.remove(key);
    }
}

//! A `System` for host tests: no SD card, no audio, no network, a clock
//! the test advances, and per-owner heap counters the test can set.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use p4os_sdk::*;

#[derive(Default)]
pub struct NoFs;

impl FileSystem for NoFs {
    fn list(&mut self, _path: &str) -> Result<Vec<DirEntry>, FsError> {
        Err(FsError::NoMedium)
    }
    fn read(&mut self, _path: &str) -> Result<Vec<u8>, FsError> {
        Err(FsError::NoMedium)
    }
    fn write(&mut self, _path: &str, _data: &[u8]) -> Result<(), FsError> {
        Err(FsError::NoMedium)
    }
    fn open(&mut self, _path: &str) -> Result<Box<dyn FileReader>, FsError> {
        Err(FsError::NoMedium)
    }
}

#[derive(Default)]
pub struct NoAudio;

impl Audio for NoAudio {
    fn play(&mut self, _source: Box<dyn AudioSource>) {}
    fn pause(&mut self, _paused: bool) {}
    fn stop(&mut self) {}
    fn state(&self) -> PlaybackState {
        PlaybackState::Stopped
    }
    fn position_frames(&self) -> u64 {
        0
    }
    fn set_volume(&mut self, _volume: f32) {}
    fn volume(&self) -> f32 {
        0.5
    }
}

#[derive(Default)]
pub struct NoNet;

impl Network for NoNet {
    fn status(&self) -> String {
        "offline".into()
    }
    fn is_up(&self) -> bool {
        false
    }
    fn fetch(&mut self, _url: &str) -> FetchId {
        1
    }
    fn poll(&mut self, _id: FetchId) -> FetchState {
        FetchState::Pending
    }
}

#[derive(Default)]
pub struct TestSys {
    pub now_ms: Cell<u64>,
    pub fs: NoFs,
    pub audio: NoAudio,
    pub net: NoNet,
    pub owner: u8,
    /// Heap attributed per owner tag (SRAM, PSRAM), set by the test.
    pub heap: [(usize, usize); 16],
    /// Free heap reported by `stats` (SRAM, PSRAM); 0 = unknown.
    pub free: (usize, usize),
    pub log: Vec<String>,
    pub config: BTreeMap<String, String>,
}

impl TestSys {
    pub fn advance(&self, ms: u64) {
        self.now_ms.set(self.now_ms.get() + ms);
    }
}

impl System for TestSys {
    fn uptime_ms(&self) -> u64 {
        self.now_ms.get()
    }
    fn now(&self) -> Option<DateTime> {
        None
    }
    fn set_time(&mut self, _t: DateTime) {}
    fn fs(&mut self) -> &mut dyn FileSystem {
        &mut self.fs
    }
    fn audio(&mut self) -> &mut dyn Audio {
        &mut self.audio
    }
    fn net(&mut self) -> &mut dyn Network {
        &mut self.net
    }
    fn stats(&self) -> SystemStats {
        let total = 1 << 30;
        let (fs, fp) = self.free;
        if fs == 0 && fp == 0 {
            return SystemStats::default();
        }
        SystemStats { heap_total: total, heap_used: total - fs, psram_total: total, psram_used: total - fp, ..Default::default() }
    }
    fn decode_image(&mut self, _data: &[u8], _max_w: i32, _max_h: i32) -> Option<Image> {
        None
    }
    fn brightness(&self) -> f32 {
        1.0
    }
    fn set_brightness(&mut self, _value: f32) {}
    fn run_command(&mut self, _line: &str) -> String {
        String::new()
    }
    fn launch(&mut self, _app_id: &str) {}
    fn config_get(&self, key: &str) -> Option<String> {
        self.config.get(key).cloned()
    }
    fn config_set(&mut self, key: &str, value: &str) {
        self.config.insert(key.into(), value.into());
    }
    fn log(&mut self, msg: &str) {
        self.log.push(msg.into());
    }
    fn set_heap_owner(&mut self, owner: u8) -> u8 {
        core::mem::replace(&mut self.owner, owner)
    }
    fn heap_owner_usage(&self, owner: u8) -> (usize, usize) {
        self.heap[owner as usize]
    }
    fn clear_heap_owner(&mut self, owner: u8) {
        self.heap[owner as usize] = (0, 0);
    }
}

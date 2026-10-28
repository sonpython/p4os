//! On-device YouTube playback for p4os.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod app;
pub mod audio;
pub mod bench;
pub mod clients;
pub mod error;
pub mod innertube;
pub mod json;
pub mod mp4;
pub mod player;
pub mod remote;
pub mod spsc;
pub mod video;
pub mod webm;
pub mod yuv;

//! The p4os desktop (launcher, status bar, keyboard) and built-in apps.
//!
//! Every app lives in its own module and is listed in [`APPS`].

#![no_std]

extern crate alloc;

pub mod desktop;
pub mod icons;
pub mod lock;

pub mod calculator;
pub mod files;
pub mod photos;
pub mod notes;
pub mod music;
pub mod browser;
pub mod terminal;
pub mod monitor;
pub mod settings;
pub mod clock;
pub mod paint;
pub mod game2048;
pub mod youtube;

use p4os_sdk::AppInfo;

pub static APPS: &[AppInfo] = &[
    calculator::INFO,
    files::INFO,
    photos::INFO,
    notes::INFO,
    music::INFO,
    browser::INFO,
    terminal::INFO,
    monitor::INFO,
    settings::INFO,
    clock::INFO,
    paint::INFO,
    game2048::INFO,
    youtube::INFO,
];

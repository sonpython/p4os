//! Hardware-independent graphics for p4os: an RGB565 canvas, anti-aliased
//! bitmap fonts, and an immediate-mode widget layer for touch screens.
//!
//! The crate is `no_std` + `alloc` so the same code runs in the kernel and
//! in the host simulator used for screenshot tests.

#![no_std]

extern crate alloc;

pub mod canvas;
pub mod color;
pub mod font;
pub mod geom;
pub mod keyboard;
pub mod ui;

pub use canvas::Canvas;
pub use color::Color;
pub use font::Font;
pub use geom::{Point, Rect};
pub use ui::{Input, Theme, Ui};

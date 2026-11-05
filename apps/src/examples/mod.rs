//! Example apps for the developer guide (`docs/app-development.md`).
//!
//! The examples are ordinary built-in apps. Instead of joining the main
//! launcher pages they form the "Examples" group, which the desktop
//! registers in `Desktop::new()` and shows on a page of its own. They build,
//! run and get tested with the rest of the workspace.
//!
//! To add an example: write a module with an `INFO: AppInfo` (see
//! [`hello`]) and list it in [`EXAMPLES`].

use p4os_sdk::AppInfo;

pub mod hello;
pub mod lap_timer;
pub mod weather;

#[cfg(test)]
pub(crate) mod testkit;

/// The examples, in the order the Examples page shows them.
pub static EXAMPLES: &[AppInfo] = &[hello::INFO, lap_timer::INFO, weather::INFO];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        for (i, a) in EXAMPLES.iter().enumerate() {
            assert!(EXAMPLES[i + 1..].iter().all(|b| b.id != a.id), "duplicate id {}", a.id);
            assert!(crate::APPS.iter().all(|b| b.id != a.id), "{} clashes with a built-in app", a.id);
        }
    }
}

//! Settings > Security: lock screen switch, passcode set/change/remove and
//! the auto-lock policy.
//!
//! Changing or removing the passcode, and switching the lock off, require
//! the current passcode; those checks count towards the same rate limit as
//! the lock screen, and reaching a lockout locks the screen. A new passcode
//! is typed twice. Switching the lock on without a passcode, or setting
//! one, leads to the new-passcode steps and turns the lock on when saved.

use super::pin::{self, PinEntry, PinHash};
use super::screen::{self, PadKey};
use super::throttle::Throttle;
use super::{AutoLock, Env, has_passcode, is_configured};
use p4os_sdk::config::{LOCK_AUTO, LOCK_ENABLED, LOCK_PIN};
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{Rect, System, Ui};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Purpose {
    Change,
    Remove,
    TurnOff,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Current(Purpose),
    New,
    Confirm,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Toggle,
    SetPasscode,
    ChangePasscode,
    RemovePasscode,
    Cancel,
    Key(PadKey),
    AutoLock(AutoLock),
}

#[derive(Default)]
pub struct SecurityPage {
    step: Option<Step>,
    entry: PinEntry,
    /// The new passcode while it is typed the second time.
    first: PinEntry,
    message: Option<&'static str>,
}

impl SecurityPage {
    /// Leave any passcode entry (e.g. when another section is opened).
    pub fn reset(&mut self) {
        self.step = None;
        self.entry.clear();
        self.first.clear();
        self.message = None;
    }

    fn start(&mut self, step: Step) {
        self.reset();
        self.step = Some(step);
    }

    fn finish(&mut self, message: &'static str) {
        self.reset();
        self.message = Some(message);
    }

    pub fn apply<E: Env + ?Sized>(&mut self, env: &mut E, action: Action) {
        match action {
            Action::Toggle if is_configured(env) => self.start(Step::Current(Purpose::TurnOff)),
            Action::Toggle if has_passcode(env) => {
                env.set(LOCK_ENABLED, "1");
                self.finish("Lock screen on");
            }
            Action::Toggle | Action::SetPasscode => self.start(Step::New),
            Action::ChangePasscode => self.start(Step::Current(Purpose::Change)),
            Action::RemovePasscode => self.start(Step::Current(Purpose::Remove)),
            Action::Cancel => self.reset(),
            Action::AutoLock(a) => env.set(LOCK_AUTO, a.value()),
            Action::Key(PadKey::Digit(d)) => self.entry.push(d),
            Action::Key(PadKey::Back) => self.entry.pop(),
            Action::Key(PadKey::Enter) => self.submit(env),
        }
    }

    fn submit<E: Env + ?Sized>(&mut self, env: &mut E) {
        match self.step {
            None => {}
            Some(Step::Current(purpose)) => self.check_current(env, purpose),
            Some(Step::New) => {
                if self.entry.is_valid() {
                    self.first = core::mem::take(&mut self.entry);
                    self.step = Some(Step::Confirm);
                    self.message = None;
                } else {
                    self.entry.clear();
                    self.message = Some("Use 4 to 8 digits");
                }
            }
            Some(Step::Confirm) => {
                if !self.entry.matches(&self.first) {
                    self.start(Step::New);
                    self.message = Some("Passcodes did not match");
                    return;
                }
                let mut salt = [0u8; pin::SALT_LEN];
                if !env.random(&mut salt) {
                    self.finish("No secure random source; not saved");
                    return;
                }
                let record = PinHash::new(&self.first, salt, pin::ITERATIONS).encode();
                env.set(LOCK_PIN, &record);
                env.set(LOCK_ENABLED, "1");
                // A new passcode starts with a clean attempt count.
                Throttle::load(env).record_success(env);
                self.finish("Passcode saved");
            }
        }
    }

    fn check_current<E: Env + ?Sized>(&mut self, env: &mut E, purpose: Purpose) {
        if self.entry.is_empty() {
            return;
        }
        let mut throttle = Throttle::load(env);
        if throttle.remaining_ms(env.uptime_ms()) > 0 {
            self.reset();
            env.request_lock();
            return;
        }
        let ok = env.get(LOCK_PIN).and_then(|v| PinHash::decode(&v)).is_some_and(|h| h.verify(&self.entry));
        self.entry.clear();
        if !ok {
            throttle.record_failure(env);
            if throttle.remaining_ms(env.uptime_ms()) > 0 {
                self.reset();
                env.request_lock();
            } else {
                self.message = Some("Wrong passcode");
            }
            return;
        }
        throttle.record_success(env);
        match purpose {
            Purpose::Change => {
                self.step = Some(Step::New);
                self.message = None;
            }
            Purpose::Remove => {
                env.set(LOCK_PIN, "");
                env.set(LOCK_ENABLED, "0");
                self.finish("Passcode removed");
            }
            Purpose::TurnOff => {
                env.set(LOCK_ENABLED, "0");
                self.finish("Lock screen off");
            }
        }
    }

    fn prompt(&self) -> &'static str {
        match self.step {
            Some(Step::Current(_)) => "Enter current passcode",
            Some(Step::New) => "Enter a new passcode (4-8 digits)",
            Some(Step::Confirm) => "Enter the new passcode again",
            None => "",
        }
    }

    /// Draw the page into `r` and act on this frame's input.
    pub fn show(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) {
        let t = *ui.theme;
        let (head, rest) = r.split_top(52);
        let title = ui.fonts.title;
        ui.text(head, "Security", &title, t.text, Align::Left);
        let r = Rect::new(rest.x, rest.y + 12, rest.w, rest.h - 12);
        let action = if self.step.is_some() { self.pad_view(ui, r) } else { self.overview(ui, r, sys) };
        if let Some(a) = action {
            self.apply(sys, a);
        }
    }

    fn overview(&mut self, ui: &mut Ui, r: Rect, sys: &mut dyn System) -> Option<Action> {
        let t = *ui.theme;
        let (body, small, large) = (ui.fonts.body, ui.fonts.small, ui.fonts.large);
        let mut action = None;
        let has_pin = has_passcode(sys);

        // Switch row.
        let p = Rect::new(r.x, r.y, r.w, 64);
        ui.panel(p);
        let mut on = is_configured(sys);
        let label = Rect::new(p.x + 18, p.y, p.w - 120, p.h);
        ui.text(Rect::new(label.x, label.y + 4, label.w, 30), "Lock screen", &large, t.text, Align::Left);
        let sub = if has_pin { "Passcode set" } else { "No passcode" };
        ui.text(Rect::new(label.x, label.y + 32, label.w, 26), sub, &small, t.text_dim, Align::Left);
        if ui.toggle(Rect::new(p.x, p.y, p.w - 18, p.h), &mut on) {
            action = Some(Action::Toggle);
        }

        // Passcode buttons.
        let row = Rect::new(r.x, p.bottom() + 10, r.w, 52);
        if has_pin {
            if ui.button(row.grid_cell(2, 1, 10, 0, 0), "Change") {
                action = Some(Action::ChangePasscode);
            }
            if ui.button_colored(row.grid_cell(2, 1, 10, 1, 0), "Remove", t.surface_alt, t.danger) {
                action = Some(Action::RemovePasscode);
            }
        } else if ui.button(row, "Set passcode") {
            action = Some(Action::SetPasscode);
        }

        // Auto-lock: one row of four in landscape, two by two in portrait.
        let current = AutoLock::load(sys);
        let cols = if r.w >= 520 { 4 } else { 2 };
        let rows = 4 / cols;
        let grid_h = rows * 52 + (rows - 1) * 8;
        let ap = Rect::new(r.x, row.bottom() + 10, r.w, 12 + 24 + 6 + grid_h + 12);
        ui.panel(ap);
        ui.text(Rect::new(ap.x + 18, ap.y + 12, ap.w - 36, 24), "Auto-lock", &body, t.text, Align::Left);
        let grid = Rect::new(ap.x + 12, ap.y + 12 + 24 + 6, ap.w - 24, grid_h);
        for (i, a) in AutoLock::ALL.into_iter().enumerate() {
            let cell = grid.grid_cell(cols, rows, 8, i as i32 % cols, i as i32 / cols);
            let (bg, fg) = if a == current { (t.accent, t.accent_text) } else { (t.surface_alt, t.text) };
            if ui.button_colored(cell, a.label(), bg, fg) && a != current {
                action = Some(Action::AutoLock(a));
            }
        }

        let note_r = Rect::new(r.x, ap.bottom() + 6, r.w, 24);
        let (text, color) = match self.message {
            Some(m) => (m, t.accent),
            None => ("Forgot it? Run `lock reset` on the serial console.", t.text_dim),
        };
        ui.text(note_r, text, &small, color, Align::Left);
        action
    }

    fn pad_view(&mut self, ui: &mut Ui, r: Rect) -> Option<Action> {
        let t = *ui.theme;
        let (large, body) = (ui.fonts.large, ui.fonts.body);
        let landscape = r.w > r.h;
        let (info, keys_r) = if landscape {
            let (info, keys) = r.split_left(r.w / 2);
            (info, keys)
        } else {
            let keys_h = (r.h - 172).min(r.w * 4 / 3).max(160);
            (Rect::new(r.x, r.y, r.w, r.h - keys_h), Rect::new(r.x + (r.w - keys_h * 3 / 4) / 2, r.bottom() - keys_h, keys_h * 3 / 4, keys_h))
        };
        let prompt = self.prompt();
        let f = if large.measure(prompt) <= info.w { large } else { body };
        ui.text(Rect::new(info.x, info.y, info.w, 36), prompt, &f, t.text, Align::Center);
        screen::dots(ui.canvas, Rect::new(info.x, info.y + 40, info.w, 36), self.entry.len(), t.text, 0);
        if let Some(m) = self.message {
            let small = ui.fonts.small;
            ui.text(Rect::new(info.x, info.y + 80, info.w, 24), m, &small, t.danger, Align::Center);
        }
        let cancel_r = Rect::new(info.x + (info.w - 160) / 2, info.y + 110, 160, 48);
        let mut action = None;
        if ui.button(cancel_r, "Cancel") {
            action = Some(Action::Cancel);
        }
        for k in screen::typed_keys(ui.input) {
            self.apply_key_now(k, &mut action);
        }
        if let Some(k) = screen::keypad(ui, keys_r, true) {
            self.apply_key_now(k, &mut action);
        }
        action
    }

    /// Edits are applied immediately; Enter is returned as the action so
    /// that it runs with access to the settings store.
    fn apply_key_now(&mut self, k: PadKey, action: &mut Option<Action>) {
        match k {
            PadKey::Digit(d) => self.entry.push(d),
            PadKey::Back => self.entry.pop(),
            PadKey::Enter => *action = Some(Action::Key(PadKey::Enter)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{MockEnv, configured};
    use super::*;
    use p4os_sdk::config::{LOCK_FAILS, LOCK_UNTIL};

    fn typed(page: &mut SecurityPage, env: &mut MockEnv, digits: &str) {
        for d in digits.bytes() {
            page.apply(env, Action::Key(PadKey::Digit(d)));
        }
        page.apply(env, Action::Key(PadKey::Enter));
    }

    fn verifies(env: &MockEnv, digits: &str) -> bool {
        let h = PinHash::decode(&env.get(LOCK_PIN).unwrap()).unwrap();
        h.verify(&super::super::tests::entry(digits))
    }

    #[test]
    fn enabling_without_passcode_leads_to_setting_one() {
        let mut env = MockEnv::default();
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::Toggle);
        assert_eq!(page.step, Some(Step::New));
        assert!(!is_configured(&env), "nothing is enabled until a passcode is saved");
        typed(&mut page, &mut env, "2580");
        assert_eq!(page.step, Some(Step::Confirm));
        typed(&mut page, &mut env, "2580");
        assert_eq!(page.step, None);
        assert_eq!(page.message, Some("Passcode saved"));
        assert!(is_configured(&env));
        assert!(verifies(&env, "2580"));
        assert!(!env.get(LOCK_PIN).unwrap().contains("2580"));
    }

    #[test]
    fn cancelling_the_setup_leaves_the_lock_off() {
        let mut env = MockEnv::default();
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::Toggle);
        typed(&mut page, &mut env, "2580");
        page.apply(&mut env, Action::Cancel);
        assert_eq!(page.step, None);
        assert!(!is_configured(&env));
        assert_eq!(env.get(LOCK_PIN), None);
    }

    #[test]
    fn new_passcode_rules() {
        let mut env = MockEnv::default();
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::SetPasscode);
        typed(&mut page, &mut env, "123");
        assert_eq!((page.step, page.message), (Some(Step::New), Some("Use 4 to 8 digits")));
        typed(&mut page, &mut env, "1234");
        typed(&mut page, &mut env, "1235");
        assert_eq!((page.step, page.message), (Some(Step::New), Some("Passcodes did not match")));
        assert_eq!(env.get(LOCK_PIN), None);
    }

    #[test]
    fn no_random_source_means_no_passcode() {
        let mut env = MockEnv { no_random: true, ..Default::default() };
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::SetPasscode);
        typed(&mut page, &mut env, "1234");
        typed(&mut page, &mut env, "1234");
        assert_eq!(env.get(LOCK_PIN), None);
        assert!(!is_configured(&env));
    }

    #[test]
    fn change_requires_the_current_passcode() {
        let mut env = configured("1111");
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::ChangePasscode);
        typed(&mut page, &mut env, "2222");
        assert_eq!(page.step, Some(Step::Current(Purpose::Change)));
        assert_eq!(page.message, Some("Wrong passcode"));
        assert_eq!(env.get(LOCK_FAILS).as_deref(), Some("1"));
        typed(&mut page, &mut env, "1111");
        assert_eq!(page.step, Some(Step::New));
        typed(&mut page, &mut env, "87654321");
        typed(&mut page, &mut env, "87654321");
        assert!(verifies(&env, "87654321"));
        assert!(!verifies(&env, "1111"));
        assert_eq!(env.get(LOCK_FAILS).as_deref(), Some("0"));
    }

    #[test]
    fn remove_and_turn_off() {
        let mut env = configured("1111");
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::Toggle);
        assert_eq!(page.step, Some(Step::Current(Purpose::TurnOff)));
        typed(&mut page, &mut env, "1111");
        assert!(!is_configured(&env));
        assert!(has_passcode(&env), "turning off keeps the passcode");
        // Back on: no passcode entry needed since one exists.
        page.apply(&mut env, Action::Toggle);
        assert!(is_configured(&env));
        page.apply(&mut env, Action::RemovePasscode);
        typed(&mut page, &mut env, "1111");
        assert!(!has_passcode(&env));
        assert!(!is_configured(&env));
    }

    #[test]
    fn guessing_in_settings_hits_the_lockout() {
        let mut env = configured("1111");
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::RemovePasscode);
        for _ in 0..5 {
            typed(&mut page, &mut env, "0000");
        }
        assert_eq!(env.lock_requests, 1);
        assert_eq!(page.step, None);
        assert_eq!(env.get(LOCK_UNTIL).as_deref(), Some("?"));
        // Still locked out: the right passcode is not even checked.
        page.apply(&mut env, Action::RemovePasscode);
        typed(&mut page, &mut env, "1111");
        assert!(has_passcode(&env));
        assert_eq!(env.lock_requests, 2);
    }

    #[test]
    fn auto_lock_choice_is_saved() {
        let mut env = MockEnv::default();
        let mut page = SecurityPage::default();
        page.apply(&mut env, Action::AutoLock(AutoLock::FiveMinutes));
        assert_eq!(AutoLock::load(&env), AutoLock::FiveMinutes);
    }
}

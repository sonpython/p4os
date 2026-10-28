# Gesture navigation, app lifecycle and resource control

Branch `worktree-agent-a660cbb810caad924`, merged with main (lock screen, TLSF heap) at `71ec4e1`.

## Outcome

- **Edge gestures** run before input reaches apps, in both orientations: a quick swipe up from the bottom goes Home and leaves the app running; swipe up and rest about 300 ms opens the app switcher; a swipe in from the left edge is Back, from the right edge Forward. Esc on the USB console is also Back. While the lock screen is up, gestures are disabled: the lock check runs first in `Desktop::frame` and returns early.
- **App switcher**: one card per running app (title plus a snapshot of its last frame), most recent first. Tap a card to switch, swipe it up to close the app, or use Close all. Tapping outside or Back returns to the previous app.
- **Lifecycle**: apps are Foreground, Background, Suspended or Stopped, with SDK callbacks `on_pause`, `on_resume`, `on_low_memory` and `on_stop`, plus `on_back` and `on_forward`. All default to no-ops; `on_back` returns false, which means "go Home".
  - Apps declare background capabilities in `AppInfo::background`: Music has AUDIO, Browser has NETWORK.
  - They request them at run time through `background_work()`. Only declared capabilities are granted.
- **Back/Forward in apps**:
  - Browser: history back and forward.
  - Files: closes the info panel, then walks the history, with a new forward stack.
  - Settings: portrait section page back to the list.
  - Notes: saves and closes the editor (and saves in `on_pause`).
  - Photos: viewer back to the grid (and `on_low_memory` drops decoded images).
  - 2048: saves its best score in `on_pause`.
- **Resource control for built-in apps**:
  - Per-app heap attribution (SRAM and PSRAM) and CPU time.
  - At most 6 apps; launching a 7th stops the least recently used one.
  - Memory budget per app; under memory pressure, background apps are asked to trim, then stopped one at a time by LRU. The foreground app is never stopped for pressure.
  - Background CPU budget.
  - Frame watchdog with a Close app / Wait dialog.
- **Isolated (U-mode) apps**: the kernel enforces an app count, a RAM quota and a CPU budget per window (throttle, then kill).
- **Task manager**: a new Apps tab in Monitor with CPU %, SRAM, PSRAM, state and a Close button. The shell gains `top`, `start`, `kill` and `swipe X1 Y1 X2 Y2 MS [hold]`.
- **Paged home screen** (added mid-task by the coordinator):
  - Full-size icons, 3x4 in portrait and 5x2 in landscape. The coordinator's example of 4x5 does not fit 112 px icons in 480x740.
  - Pages follow the finger and snap on release, by speed or past halfway. Page dots sit above the home bar.
  - On the home screen, edge Back goes to the previous page and Forward to the next; on the first page Back does nothing.
  - The page is kept across app switches.
  - App groups: `Desktop::register_app(group, &AppInfo)` and `register_group(group, &[AppInfo])`. Every non-main group starts on its own page, titled with the group name. This is the integration point for the "Examples" worker.

## Design

| Piece | Where | Notes |
|---|---|---|
| Gesture recogniser | `apps/src/wm/gesture.rs` | Pure state machine over (pos, down, fingers, t). See the gesture rules below. |
| Lifecycle | `apps/src/wm/lifecycle.rs` | Transition table, so each state change knows which callbacks to run. Suspended→Background is invalid (a suspended app has no CPU to ask with). |
| Policy | `apps/src/wm/policy.rs` | `Limits`; LRU `victim` (suspended before background, never foreground); `plan` (trim, grace period, then stop); `CpuMeter` (background budget); `HangWatch`; `TickBudget` (the U-mode CPU budget, shared with the kernel so it is host-tested). |
| Pager, launcher | `apps/src/wm/pager.rs`, `launcher.rs` | Pure paging state machine; catalogue, groups, grid, page split. |
| Switcher, feedback, thumbnails | `apps/src/wm/switcher.rs`, `feedback.rs`, `thumb.rs` | Snapshots are 1/3-scale box-filtered images. Each has at least 64 KiB of capacity so the heap puts it in PSRAM: at most 79 KB each, 6 apps, about 480 KB. |
| App manager | `apps/src/desktop.rs` | See the app manager notes below. |
| Heap owner tags | `kernel/src/memtag.rs` + about 10 lines in `heap.rs` | Each thread carries a tag. `GlobalAlloc::alloc`/`dealloc` (and `alloc_sram`) charge or credit `layout.size()` to the running thread's tag, through a lock-free per-core "running thread" atomic added to `sched.rs`. |
| Audio attribution | `kernel/src/audio.rs` | The audio thread runs the app's decoder, and drops a finished source, under the tag of the app that queued it. |
| U-mode supervisor | `kernel/src/appctl.rs` | See the supervisor notes below. |
| Kill path | `kernel/src/usermode.rs` | `request_kill(tid)` sets a flag and interrupts the thread. `before_resume` (now `&mut TrapFrame`) rewrites the U-mode frame in place into an M-mode frame running the existing kill path. `trap.rs` is unchanged. |
| Shell, GUI | `kernel/src/shell.rs`, `kernel/src/gui.rs`, `kernel/src/touch.rs` | Scripted touch path (tap and swipe, with the end point always delivered). GT911 finger count goes into `Input::fingers`. Built-in app usage is published once a second. `gui::busy_ms()` reports a hung frame. An animation fps log. |

**Gesture rules**
- Edge zones are 24 px wide. Inward travel of 90 px, or a flick of at least 0.6 px/ms over at least 36 px, commits a gesture.
- The switcher needs a rest of 300 ms within 14 px, after at least 70 px of travel.
- A touch in an edge zone is held back until it is decided. A tap, a 450 ms rest, or movement along the edge is replayed to the app from where it started.
- A second finger cancels the gesture.
- Touches that start elsewhere are never looked at, so in-app scrolling is unaffected.

**App manager (desktop)**
- Each running app has a tag, its own `UiState`, `CpuMeter`, `HangWatch` and snapshot.
- Every call into an app runs through `call()`, which sets the heap owner tag and measures the time.
- Once a second it rolls the CPU windows, reads the per-tag heap figures and applies `policy::plan`.
- A 24 px home-bar strip at the bottom is reserved, so app content never sits under the bottom gesture zone.
- Each home screen page's static layer is cached.

**U-mode supervisor (`appctl.rs`)**
- Limits: 4 apps, 192 KiB of RAM each and 512 KiB in total (checked at load), and 25% of one core per 1 s window.
- The `appmon` thread samples every 20 ms. When an app has used its budget, `sched::hold_until` keeps it off the CPU until the window ends.
- An app throttled for 10 windows in a row is killed through `usermode::request_kill`.

## Measurements (board, 360 MHz)

| | Render | Present | Rate |
|---|---|---|---|
| Home screen page drawn from scratch (12 AA icons + gradient) | 278 ms | 25 ms | 3.3 fps, before caching |
| Bottom/edge gesture tracking over Calculator or Monitor | 17–27 ms | 24–28 ms | 18–24 fps |
| Switcher opening animation from home | 17.6 ms | 23.9 ms | 24.4 fps |
| Swipe home with shrink animation (cached page, run 3) | 27 ms avg, 54 ms worst | 23 ms | 19 fps |
| Edge Back out of the switcher (run 3) | 21 ms | 27 ms | 19.8 fps |

- **Why the home layer is cached.** The first board run showed the home screen rendering in 278 ms. That made the touch sampling on the home screen so slow that an injected swipe collapsed into a tap. The page's static layer is now cached (about 710 KB per page in PSRAM, dropped under memory pressure) and the running-app dots are drawn on top. `blit_scaled` also no longer does a 64-bit division per pixel.
- **The one remaining slow frame.** After that fix, one frame of about 314 ms remained when going home, caused by rebuilding the cache after the running set changed. Moving the dots out of the cache removed it; this was confirmed in the simulator, and the board re-check is listed under Board verification.
- **Present cost.** The ~25 ms of present (blit and DSI flip, existing kernel code) now caps the rate below 30 fps.
- **Simulator.** Renders take 0.5–1.5 ms per gesture frame.

Per-app heap figures read on the board in `top`: Calculator 0 KiB, Monitor 1 KiB SRAM. In the simulator's tagged allocator, Music playing in the background shows 31.8 KB (its decoder) and Browser 13.9 KB.

## Board verification

Every board action ran inside `scripts/board-lock.sh`.

Run 2 (kernel `ef9408d` plus the paging fixes):
- `tap` started Calculator.
- `swipe 240 795 240 560 150` went home, with Calculator suspended.
- Monitor opened, and its Apps tab was captured on camera.
- `swipe ... hold` opened the switcher, with both cards captured on camera.
- An edge `swipe 3 400 200 400 150` returned from the switcher to Monitor, and a second one went to the home screen.
- A swipe and hold on the home screen opened the switcher.
- `top` listed both apps with their states and CPU ms.

Run 3 (merged main with the TLSF heap, plus the `crash 10` spin app in slot 13):
- The gestures work again on the merged kernel, and the home animation no longer has a slow frame (worst 54 ms, down from 314 ms).
- `start 13 10` started the spin app. It was throttled straight away: `top` showed `t7 crash bg 26.0%  throttled` and `ps` showed core 1 at 26%.
- After 10 s: `appmon: 'crash' (thread 7) exceeded its CPU budget for 10 s, killing it`, then `*** app 'crash' (thread 7) killed by the app supervisor`. Afterwards the kernel, GUI and shell kept running, `top` was clean, and `mem` showed SRAM 205 KB of 446 KB used.

Camera captures are in `out/board-nav/` (not committed).

Run 1 surprises:
- `cam.py` reset the board, because my session script left DTR low. Opening the port like `board-ui.py` does fixed it.
- The launcher had 3 columns on a 480 px screen, not 4.

## Tests

`cargo test -p p4os-apps -p p4os-gui -p p4os-net -p p4os-sdk --target aarch64-unknown-linux-gnu`: 174 + 2 + 8 + 10 pass, with zero warnings. The kernel (`RUSTFLAGS="-C link-arg=-Tlink.x"`) and the simulator build with zero warnings.

New host tests cover:
- the gesture recogniser (19 cases): flick vs. slow, hold, hold then flick, multi-touch, taps and long presses replayed, scrolls not stolen, corners, landscape;
- lifecycle transitions;
- LRU, budget, pressure, CPU-meter, hang and tick-budget policy;
- the pager (8) and the launcher (4);
- thumbnails and `blit_scaled`;
- Files back/forward;
- the Monitor rows;
- 15 desktop integration tests: Home keeps the app, the switcher (open, tap, swipe up to close, Close all), Back at the root goes home, Escape, edge taps, gestures disabled while locked, the app limit, the over-budget trim-then-kill, memory pressure sparing the foreground app, paging, and edge Back/Forward turning pages.

Screenshots are in `docs/screenshots/nav-*.png`, generated by `cargo sim -- --script docs/screenshots/nav-shots.txt`.

## Limits and known gaps

- **Heap attribution is per running thread**, not per block, so there is no allocator header (a hard constraint while TLSF was being rewritten):
  - Memory allocated by one owner and freed by another moves between counters. For example, a network response body is allocated by the net thread and freed by the Browser.
  - A counter can go negative (it reads as 0, a lenient under-count) or drift high.
  - Known high-drift paths (the audio source) are routed to the right owner, and a tag is cleared when its app stops.
  - Budgets are therefore deliberately generous: 160 KiB SRAM and 8 MiB PSRAM per app, and the foreground app is stopped only at twice that.
  - Pool stacks that fall back to PSRAM are uncharged at allocation but credited on free. This is a system-tag-only drift.
- **A foreground frame that never returns cannot be recovered by the desktop.** It runs on the GUI thread. The soft watchdog only reacts to frames that return late (over 1 s, 3 times in 60 s). A true hang shows in `top` via `gui::busy_ms()` and is left to the fault worker's 15 s `wdt::watch("gui")`.
- **Background CPU for built-in apps is measured, not preempted**: they run inside the GUI thread. Over-budget background ticks are skipped for the rest of the window, and an app over budget for 5 windows in a row is suspended.
- **U-mode RAM is checked at load only**, because there is no allocation syscall yet. A future `sbrk` should charge `appctl`'s quota. The CPU budget can overrun by up to `CHECK_MS` (20 ms) per window.
- **Home transition animation**: when the app's icon is on another page, it shrinks into the middle of the page.

## Coordination notes

- The lock branch and later main (lock screen, TLSF heap) are merged.
- The heap hook was re-applied onto TLSF as 3 small edits in `alloc`, `alloc_sram` and `dealloc`.
- `trap.rs` is untouched.
- `sched.rs` has small additions only: `running_id`, `run_ticks`, `hold_until`, `interrupt`, and `memtag::reset_thread` on spawn.
- The power worker can call `Desktop::on_screen_off` / `on_wake`, which come from the lock work.
- `Desktop::set_gestures_enabled(bool)` exists for any other modal owner.
- The YouTube branch's icon shrinking should be dropped in favour of paging.
- The Examples worker should call `desktop.register_group("Examples", &EXAMPLES)` after `Desktop::new()`, in both the kernel `gui.rs` and the simulator.
- I added `crash` test 10 (spin forever) for the CPU budget and flashed it to slot 13.

## Unresolved questions

- Should System's frame-time stats (`last_frame_us`/`last_present_us`) feed the Monitor task manager, so it can show per-app frame times?
- Where should the Examples group be registered: inside `Desktop::new()` (apps crate) or by the kernel and simulator? The API supports both.

# YouTube app (official API part): implementation report

Date: 2026-10-28. Branch: worktree `agent-a31cf192d7173ac37`. Status: done, verified on host and simulator only.

## What was built

A built-in `youtube` app (`apps/src/youtube/`) with tabs Home / Search / Library / Account, a video detail page, and a playback seam.

| Module | Role |
|---|---|
| `oauth.rs` | Google device authorization flow: device code, polling with `interval`/`slow_down`, `authorization_pending`, `access_denied`, `expired_token`, refresh with backoff, `invalid_grant` ends the session, sign-out revokes. `Debug` output redacts every token and the client secret. |
| `client.rs` | Sends Data API calls with a bearer token (signed in) or `key=` (signed out), queues calls while a token refreshes, max 3 in flight, one refresh-and-retry on 401, stops on `quotaExceeded`. |
| `quota.rs` | Daily budget (10,000 units, Pacific day taken as UTC-8), search 100 / lists 1, persisted as `youtube.quota` every 100 units and on close. Cost table in the module docs and in `docs/getting-started.md`. |
| `feed.rs` | Subscriptions feed: subscriptions -> channels (uploads playlists) -> playlistItems per channel -> videos (stats). About 23 units per refresh with 20 channels, cached 15 min. |
| `api.rs` | Call builders with `fields=` partial responses, parsers, ISO 8601 durations, counts, relative ages, HTML entity decoding for search titles, ID sanitising before anything goes into a URL. |
| `json.rs` | Hand-written zero-copy JSON reader: validates once, then lazy lookups over the response slice; no allocation except decoded strings; depth limit 32. Chosen over `serde-json-core` (needs fixed structs, cannot skip large unknown parts cheaply) and `miniserde` (full DOM, several times the input size). |
| `thumbs.rs` | Thumbnail LRU (40 decoded 160x90 images, ~1.1 MB), fetches only rows on screen, max 2 downloads, decodes one JPEG per frame and none while touching, via the existing `media` decoder. |
| `qr.rs` | QR codes via `qrcodegen-no-heap` (no_std, MIT), drawn with a quiet zone. |
| `player.rs` | `Player` trait and `player::open(video)`, the single seam for future playback. Today returns `Unavailable`: "Playback not available yet" plus a QR of `https://youtu.be/<id>`. |

## Shared changes (minimal, backward compatible)

- `net/src/http.rs`: `http::Request { method, headers, body }` with `validate()` (CR/LF injection, reserved headers, 2 KiB body cap), `encode()`, `follow_redirect()` (303-style downgrade; Authorization dropped on cross-origin redirects). `Url::request()` output is byte-identical to before (tested).
- `sdk/src/lib.rs`: `HttpRequest`, `Network::send` (default: plain GETs only, anything else fails loudly), `System::config_remove` (default stores an empty value).
- `sdk/src/config.rs`: `youtube.*` keys; `is_hidden()` for values readable by apps but never printed.
- `kernel/src/net.rs`: the request queue carries an `http::Request`; `WifiNet::send`. `kernel/src/shell.rs`: `config set KEY VALUE`, `config unset KEY`, hidden values echoed as `*` while typing and shown as `[set]`; line buffer 128 -> 192. `kernel/src/gui.rs`: `config_remove`.
- `apps/src/desktop.rs`: forwards `config_remove`; launcher icons shrink to fit when rows overflow (13 apps no longer fit in portrait; landscape already overflowed with 12).
- `sim/`: `HostNet::send`, script commands `config KEY VALUE` and `mock URL_PART FILE [STATUS]`.

## Verification

- `cargo test -p p4os-apps -p p4os-gui -p p4os-net -p p4os-sdk --target aarch64-unknown-linux-gnu`: 151 tests pass, zero warnings (58 new in the app, 4 new in net, 1 new in sdk). App-level tests drive `update()` end to end with a mock network: signed-out trending -> detail -> player, sign-in storing the refresh token and sign-out deleting it, the full feed from fixtures with quota saved, expired session deleting the token, a channel page loading under an open video.
- `cargo build --release -p p4os-sim --target aarch64-unknown-linux-gnu` and `RUSTFLAGS="-C link-arg=-Tlink.x" cargo build --release -p p4os-kernel`: both succeed.
- Screenshots from hand-made fixtures (`cargo sim --script sim/fixtures/youtube/screens.sim`): `docs/screenshots/youtube-{trending,device-code,feed,detail,player,search,library,landscape,landscape-detail}.png`. Thumbnails are synthetic (`sim/fixtures/youtube/make-thumbs.py`).

## Not verified (needs the board and a real Google project)

- Real sign-in against Google (no credentials were used). Fixtures follow the documented response shapes; Google returns `verification_url` (RFC 8628's `verification_uri` is also accepted).
- TLS on the board to `oauth2.googleapis.com`, `www.googleapis.com`, `i.ytimg.com` (Google Trust Services, ECDSA) and that Google answers HTTP/1.0 without chunked encoding.
- Thumbnail decode time per frame on the 360 MHz core, scroll smoothness, and PSRAM/SRAM peaks with 3 API calls + 2 thumbnail downloads in flight (5 concurrent TLS sessions).
- Flash writes of `youtube.refresh_token` / `youtube.quota` from the GUI thread (same path the Settings app already uses).
- The rendered QR codes were not scanned (no decoder on the host); the encoder is Nayuki's library and the render test checks finder patterns and the quiet zone.

## Merge with main (d7905b9: TLSF heap, user-mode apps, lock screen)

- One masking list: `config::is_masked()` (main's name) covers `wifi.password`, every `lock.*` key, `youtube.client_secret`, `youtube.api_key` and `youtube.refresh_token`. `config list`, `config set` output and the typed-value echo all use it. My `is_hidden()` was folded into it.
- `config::is_protected()`: `config set` and `config unset` refuse `lock.*`, so the passcode only changes through Settings or `lock reset`.
- Both SDK hooks kept: `fill_random`/`lock_now` from main, `config_remove` from this branch; desktop proxy, kernel and simulator implement all of them. The simulator's `config` command keeps main's frame-after-set and accepts values with spaces; `lock` and `mock` both exist.
- New `Network::cancel(id)` (default no-op). Kernel: `net::cancel` removes the result slot and any queued request; the net thread drops jobs whose slot is gone and closes their sockets; `finish` no longer recreates a cancelled slot. The YouTube app cancels API, token and thumbnail requests in `on_close`; the Browser cancels the page it was loading.
- Home without an API key or account now shows "No API key set" with the exact `config set youtube.api_key <KEY>` command (`docs/screenshots/youtube-no-key.png`).
- Host: 191 tests pass across the four crates, zero warnings. Kernel (`RUSTFLAGS="-C link-arg=-Tlink.x"`) and simulator release builds are clean.

## Board smoke test (merged kernel, 2026-10-28 17:31 UTC, inside scripts/board-lock.sh)

No real credentials were set; the only test value (`youtube.client_secret smoke-test`) was removed again.

- Boot: Wi-Fi, DHCP and SNTP OK; `lock` reports off, no passcode.
- `config set lock.auto ...` and `config unset lock.pin` are refused ("can only be changed in Settings or with `lock reset`"); the typed values echo as `*`.
- `config set youtube.client_secret smoke-test` echoes `**********`, answers `youtube.client_secret = [set]`, `config list` shows `[set]`; `config unset` removes it.
- Tapping the 13th launcher icon opens YouTube; the camera capture shows "No API key set", the `config set youtube.api_key` hint, the Account button and the tab bar. `ps` afterwards: GUI and net threads healthy, no app-side network activity without a key.
- Not exercised on the board: real API calls, sign-in and request cancellation with live connections (cancel is covered by host tests and the kernel build).
- The serial helper used for slow typing now has a write timeout, and every step of the smoke script runs under `timeout`, after an earlier version hung while holding the board lock.

## Known limitations

- Search in landscape with the keyboard open leaves little room for results.
- Typing `config set` through the Terminal app shows the value in the Terminal's own input line (the serial echo is masked).
- Long shell lines written to the serial port in one burst lose characters (seen with `shell-session.py`); typed at human speed, or one character per 10 ms, they arrive intact.

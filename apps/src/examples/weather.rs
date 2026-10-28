//! Current weather from Open-Meteo (free, no API key) over HTTPS.
//!
//! Shows:
//! * starting a background request with [`Network::fetch`] and polling it
//!   with [`Network::poll`] from `update`, which never blocks;
//! * asking for ticks only while a request is in flight;
//! * keeping the finished result, because the OS hands it out only once;
//! * turning every failure (offline, network error, HTTP error status,
//!   unexpected body) into a message and a Retry button;
//! * reading a few fields from a JSON response without a JSON library.
//!
//! [`Network::fetch`]: p4os_sdk::Network::fetch
//! [`Network::poll`]: p4os_sdk::Network::poll

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, FetchId, FetchState, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "example-weather", name: "Weather", icon, create };

fn create() -> Box<dyn App> {
    Box::new(WeatherApp::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x60a5fa), Color::hex(0x1d4ed8));
    let sun = Point::new(r.x + r.w * 5 / 8, r.y + r.h * 3 / 8);
    c.fill_circle(sun, r.w as f32 * 0.18, Color::hex(0xfacc15));
    let cloud = Color::WHITE;
    let base = r.y + r.h * 5 / 8;
    c.fill_circle(Point::new(r.x + r.w * 3 / 8, base), r.w as f32 * 0.16, cloud);
    c.fill_circle(Point::new(r.x + r.w / 2, base - r.h / 12), r.w as f32 * 0.18, cloud);
    c.fill_round_rect(Rect::new(r.x + r.w / 4, base, r.w / 2, r.h / 7), r.h / 14, cloud);
}

pub struct City {
    pub name: &'static str,
    pub latitude: f32,
    pub longitude: f32,
}

pub const CITIES: [City; 4] = [
    City { name: "London", latitude: 51.51, longitude: -0.13 },
    City { name: "Hanoi", latitude: 21.03, longitude: 105.85 },
    City { name: "New York", latitude: 40.71, longitude: -74.01 },
    City { name: "Tokyo", latitude: 35.68, longitude: 139.69 },
];

/// The chosen city (an index into [`CITIES`]) is remembered.
pub const CITY_KEY: &str = "example-weather.city";

/// The request for one city. Only public data goes into the URL: never put
/// passwords or tokens in a URL, since URLs end up in logs and error
/// messages.
pub fn forecast_url(city: &City) -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={:.2}&longitude={:.2}\
         &current=temperature_2m,relative_humidity_2m,wind_speed_10m,weather_code",
        city.latitude, city.longitude
    )
}

#[derive(Clone, Debug, PartialEq)]
pub struct Weather {
    pub temperature_c: f32,
    pub humidity_pct: f32,
    pub wind_kmh: f32,
    pub code: u32,
    /// Observation time from the response, "HH:MM" in UTC.
    pub time: String,
}

/// Turn a finished HTTP response into weather or a message for the user.
pub fn parse_weather(status: u16, body: &[u8]) -> Result<Weather, String> {
    let text = core::str::from_utf8(body).map_err(|_| "The server sent something that is not text.".to_string())?;
    if status != 200 {
        // Open-Meteo explains errors as {"error":true,"reason":"..."}.
        let reason = json_field(text, "reason").and_then(json_str).unwrap_or("");
        return Err(format!("The server answered HTTP {}. {}", status, reason).trim_end().to_string());
    }
    let parsed = (|| {
        let current = json_field(text, "current")?;
        let number = |key| json_field(current, key)?.parse::<f32>().ok();
        let time = json_str(json_field(current, "time")?)?;
        Some(Weather {
            temperature_c: number("temperature_2m")?,
            humidity_pct: number("relative_humidity_2m")?,
            wind_kmh: number("wind_speed_10m")?,
            code: json_field(current, "weather_code")?.parse().ok()?,
            time: time.split_once('T').map_or(time, |(_, hm)| hm).to_string(),
        })
    })();
    parsed.ok_or_else(|| "The response did not contain the expected weather fields.".to_string())
}

/// Plain-language text for a WMO weather code.
pub fn describe(code: u32) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 | 53 | 55 => "Drizzle",
        56 | 57 => "Freezing drizzle",
        61 | 63 | 65 => "Rain",
        66 | 67 => "Freezing rain",
        71 | 73 | 75 => "Snow",
        77 => "Snow grains",
        80..=82 => "Rain showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown conditions",
    }
}

/// Raw text of the value stored under `key` in the JSON object `obj`
/// (its own keys only, not nested ones). This is not a full JSON parser,
/// just enough for well-formed API responses; malformed input gives `None`
/// and never panics.
pub fn json_field<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let b = obj.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i) == Some(&b'}') {
            return None;
        }
        let key_end = string_end(b, i)?;
        let found = &obj[i + 1..key_end - 1] == key;
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let value_end = value_end(b, i)?;
        if found {
            return Some(&obj[i..value_end]);
        }
        i = skip_ws(b, value_end);
        if b.get(i) != Some(&b',') {
            return None;
        }
        i += 1;
    }
}

/// The contents of a JSON string value without escapes, e.g. `"14:30"`.
pub fn json_str(value: &str) -> Option<&str> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('\\')).then_some(inner)
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        i += 1;
    }
    i
}

/// Index just past the string starting at `i`.
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Index just past the value starting at `i`.
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let len = b[i..].iter().position(|c| b",}] \t\r\n".contains(c)).unwrap_or(b.len() - i);
            (len > 0).then_some(i + len)
        }
    }
}

/// "15.1°C" in the huge font, centred in `r`. The bundled fonts have no
/// degree sign, so it is drawn as a small ring.
fn draw_temperature(ui: &mut Ui, r: Rect, celsius: f32) {
    let t = *ui.theme;
    let huge = ui.fonts.huge;
    let number = format!("{:.1}", celsius);
    let (num_w, c_w, ring) = (huge.measure(&number), huge.measure("C"), 16);
    let x = r.x + (r.w - num_w - ring - c_w) / 2;
    ui.text(Rect::new(x, r.y, num_w, r.h), &number, &huge, t.text, Align::Left);
    let ring_y = r.y + (r.h - huge.line_height) / 2 + huge.line_height / 4;
    ui.canvas.stroke_circle(Point::new(x + num_w + ring / 2, ring_y), 5.0, 2.5, t.text);
    ui.text(Rect::new(x + num_w + ring, r.y, c_w + 2, r.h), "C", &huge, t.text, Align::Left);
}

enum Load {
    /// Nothing requested yet.
    Idle,
    Waiting(FetchId),
    Ready(Weather),
    Failed(String),
}

struct WeatherApp {
    city: usize,
    load: Load,
    loaded_settings: bool,
}

impl Default for WeatherApp {
    fn default() -> Self {
        Self { city: 0, load: Load::Idle, loaded_settings: false }
    }
}

impl WeatherApp {
    fn request(&mut self, sys: &mut dyn System) {
        // One request at a time. A request that is no longer wanted is
        // cancelled: otherwise the OS would keep its result until polled.
        self.cancel(sys);
        self.load = if sys.net().is_up() {
            Load::Waiting(sys.net().fetch(&forecast_url(&CITIES[self.city])))
        } else {
            Load::Failed("No network. Join a Wi-Fi network in Settings, then tap Retry.".into())
        };
    }

    fn cancel(&mut self, sys: &mut dyn System) {
        if let Load::Waiting(id) = self.load {
            sys.net().cancel(id);
            self.load = Load::Idle;
        }
    }

    /// Check on the request in flight; never waits.
    fn poll(&mut self, sys: &mut dyn System) {
        let Load::Waiting(id) = self.load else { return };
        self.load = match sys.net().poll(id) {
            FetchState::Pending => return,
            FetchState::Done { status, body, .. } => match parse_weather(status, &body) {
                Ok(w) => Load::Ready(w),
                Err(msg) => Load::Failed(msg),
            },
            // The OS reports DNS, TLS and timeout failures (a request gives
            // up after 30 s) here as text.
            FetchState::Failed(msg) => Load::Failed(format!("Could not reach the weather service: {}", msg)),
        };
    }
}

impl App for WeatherApp {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        if !self.loaded_settings {
            self.loaded_settings = true;
            let saved = sys.config_get(CITY_KEY).and_then(|v| v.parse::<usize>().ok());
            self.city = saved.filter(|&i| i < CITIES.len()).unwrap_or(0);
        }
        if matches!(self.load, Load::Idle) {
            self.request(sys);
        }
        self.poll(sys);

        let t = *ui.theme;
        ui.fill(area, t.bg);
        let inner = area.inset(20);
        let landscape = inner.w > inner.h;
        let (chips, rest) = inner.split_top(if landscape { 64 } else { 140 });
        let (cols, rows) = if landscape { (4, 1) } else { (2, 2) };
        let mut picked = None;
        for (i, city) in CITIES.iter().enumerate() {
            let cell = chips.grid_cell(cols, rows, 12, i as i32 % cols, i as i32 / cols);
            let clicked = if i == self.city { ui.button_primary(cell, city.name) } else { ui.button(cell, city.name) };
            if clicked {
                picked = Some(i);
            }
        }
        if let Some(i) = picked.filter(|&i| i != self.city) {
            self.city = i;
            sys.config_set(CITY_KEY, &format!("{}", i));
            self.request(sys);
        }

        // Portrait: the card above a Refresh button. Landscape is too low
        // for that, so the button moves to the right of the card.
        let (card, button) = if landscape {
            let (card, side) = rest.split_left(rest.w - 220);
            let button = Rect::new(side.x, side.y + 16 + (side.h - 16 - 76) / 2, side.w, 76);
            (Rect::new(card.x, card.y + 16, card.w - 16, card.h - 16), button)
        } else {
            let (card, footer) = rest.split_top(rest.h - 96);
            let button = Rect::new(footer.x + footer.w / 4, footer.y + 10, footer.w / 2, 76);
            (Rect::new(card.x, card.y + 20, card.w, card.h - 40), button)
        };
        ui.panel(card);
        let card = card.inset(16);
        let (large, body, small) = (ui.fonts.large, ui.fonts.body, ui.fonts.small);
        match &self.load {
            Load::Idle | Load::Waiting(_) => {
                ui.text(card, "Loading...", &large, t.text_dim, Align::Center);
            }
            Load::Ready(w) => {
                let (temp, details) = if landscape { card.split_left(card.w / 2) } else { card.split_top(card.h / 2) };
                draw_temperature(ui, temp, w.temperature_c);
                let lines = [
                    (describe(w.code).to_string(), &large, t.text),
                    (format!("Humidity {:.0}%", w.humidity_pct), &body, t.text_dim),
                    (format!("Wind {:.0} km/h", w.wind_kmh), &body, t.text_dim),
                    (format!("Observed {} UTC", w.time), &body, t.text_dim),
                    // Open-Meteo's free tier asks for attribution.
                    ("Weather data by Open-Meteo.com".to_string(), &small, t.text_dim),
                ];
                let row = 32;
                let top = details.y + (details.h - lines.len() as i32 * row) / 2;
                for (i, (line, font, color)) in lines.iter().enumerate() {
                    let r = Rect::new(details.x, top + i as i32 * row, details.w, row);
                    ui.text(r, line, font, *color, Align::Center);
                }
            }
            Load::Failed(msg) => {
                let lines = body.wrap(msg, card.w);
                let top = card.y + (card.h - lines.len() as i32 * 30) / 2;
                for (i, line) in lines.iter().enumerate() {
                    ui.text(Rect::new(card.x, top + i as i32 * 30, card.w, 30), line, &body, t.danger, Align::Center);
                }
            }
        }

        let label = if matches!(self.load, Load::Failed(_)) { "Retry" } else { "Refresh" };
        if ui.button(button, label) {
            self.request(sys);
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        // Poll a few times a second while waiting; otherwise sleep until
        // the user touches something.
        matches!(self.load, Load::Waiting(_)).then_some(250)
    }

    fn on_close(&mut self, sys: &mut dyn System) {
        // Leaving while a request is in flight: tell the OS to drop it.
        self.cancel(sys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::examples::testkit::Harness;

    const SAMPLE: &str = r#"{"latitude":51.51147,"longitude":-0.13078308,"utc_offset_seconds":0,
        "current_units":{"time":"iso8601","temperature_2m":"°C","weather_code":"wmo code"},
        "current":{"time":"2026-10-28T16:30","interval":900,"temperature_2m":15.4,
        "relative_humidity_2m":72,"wind_speed_10m":13.0,"weather_code":2}}"#;

    #[test]
    fn reads_json_fields() {
        assert_eq!(json_field(r#"{"a":1,"b":"x"}"#, "b"), Some(r#""x""#));
        assert_eq!(json_field(r#" { "a" : [1,{"b":2}] , "b" : -3.5e1 } "#, "b"), Some("-3.5e1"));
        assert_eq!(json_field(r#"{"a":{"b":2}}"#, "b"), None, "nested keys are not top-level");
        assert_eq!(json_field(r#"{"s":"a\"}b","k":true}"#, "k"), Some("true"));
        assert_eq!(json_str(r#""14:30""#), Some("14:30"));
        assert_eq!(json_str("14"), None);
    }

    #[test]
    fn malformed_json_gives_none() {
        for bad in ["", "[]", "{", r#"{"a""#, r#"{"a":}"#, r#"{"a":1"#, r#"{"a":"open"#, r#"{"a":1 "b":2}"#, r#"{"a":[1,2"#] {
            assert_eq!(json_field(bad, "b"), None, "{bad}");
        }
    }

    #[test]
    fn parses_an_open_meteo_response() {
        let w = parse_weather(200, SAMPLE.as_bytes()).unwrap();
        assert_eq!(w, Weather { temperature_c: 15.4, humidity_pct: 72.0, wind_kmh: 13.0, code: 2, time: "16:30".into() });
        assert_eq!(describe(w.code), "Partly cloudy");
    }

    #[test]
    fn explains_failures() {
        let err = parse_weather(400, br#"{"error":true,"reason":"Latitude must be in range"}"#).unwrap_err();
        assert_eq!(err, "The server answered HTTP 400. Latitude must be in range");
        assert_eq!(parse_weather(503, b"<html>busy</html>").unwrap_err(), "The server answered HTTP 503.");
        assert!(parse_weather(200, b"{}").unwrap_err().contains("expected weather fields"));
        assert!(parse_weather(200, &[0xff, 0xfe]).unwrap_err().contains("not text"));
    }

    #[test]
    fn url_has_the_coordinates() {
        let url = forecast_url(&CITIES[1]);
        assert!(url.starts_with("https://api.open-meteo.com/v1/forecast?latitude=21.03&longitude=105.85&current="));
    }

    #[test]
    fn fetches_shows_and_survives_errors() {
        let mut h = Harness::new();
        let mut app = WeatherApp::default();

        // Offline: no request, a message instead.
        h.frame(&mut app);
        assert!(h.sys.net.requested.is_empty());
        assert!(matches!(&app.load, Load::Failed(m) if m.starts_with("No network")));
        assert_eq!(app.tick_interval_ms(), None);

        // Online: Retry starts a request and the app asks for ticks.
        h.sys.net.up = true;
        let retry = Rect::new(20, 20, 440, 760).split_top(140).1;
        let retry = Rect::new(retry.x + retry.w / 4, retry.bottom() - 96 + 10, retry.w / 2, 76).center();
        h.tap(&mut app, retry);
        assert_eq!(h.sys.net.requested.len(), 1);
        assert_eq!(app.tick_interval_ms(), Some(250));

        // The result is kept after the one poll that delivers it.
        h.sys.net.complete_last(FetchState::Done { status: 200, content_type: "application/json".into(), body: SAMPLE.into() });
        h.frame(&mut app);
        h.frame(&mut app);
        assert!(matches!(&app.load, Load::Ready(w) if w.code == 2));
        assert_eq!(app.tick_interval_ms(), None);

        // A transport error becomes a message, not a panic.
        h.tap(&mut app, retry);
        h.sys.net.complete_last(FetchState::Failed("timed out connecting (api.open-meteo.com)".into()));
        h.frame(&mut app);
        assert!(matches!(&app.load, Load::Failed(m) if m.contains("timed out")));
    }

    #[test]
    fn remembers_the_city() {
        let mut h = Harness::new();
        h.sys.net.up = true;
        h.sys.config.insert(CITY_KEY.into(), "3".into());
        let mut app = WeatherApp::default();
        h.frame(&mut app);
        assert_eq!(app.city, 3);
        assert!(h.sys.net.requested[0].contains("latitude=35.68"));

        // Switching city while a request is in flight cancels it and
        // asks for the new city.
        let chips = Rect::new(20, 20, 440, 140);
        h.tap(&mut app, chips.grid_cell(2, 2, 12, 0, 0).center());
        assert_eq!(app.city, 0);
        assert_eq!(h.sys.config.get(CITY_KEY).map(String::as_str), Some("0"));
        assert_eq!(h.sys.net.requested.len(), 2);
        assert_eq!(h.sys.net.cancelled, [1]);
        assert!(h.sys.net.requested[1].contains("latitude=51.51"));

        // Closing the app cancels the request still in flight.
        app.on_close(&mut h.sys);
        assert_eq!(h.sys.net.cancelled, [1, 2]);

        // A stored value out of range falls back to the first city.
        h.sys.config.insert(CITY_KEY.into(), "42".into());
        let mut app = WeatherApp::default();
        h.frame(&mut app);
        assert_eq!(app.city, 0);
    }
}

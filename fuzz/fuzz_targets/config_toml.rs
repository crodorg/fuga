#![no_main]
//! Fuzz the user-editable config parser: malformed TOML must error, never panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = toml::from_str::<fuga::config::Config>(s);
    }
});

#![no_main]
//! Fuzz the SomaFM `channels.json` parser (untrusted network input).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = fuga::source::somafm::parse_channels(s);
    }
});

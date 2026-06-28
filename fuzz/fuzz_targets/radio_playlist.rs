#![no_main]
//! Fuzz the .pls/.m3u playlist parsers (untrusted network input).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = fuga::source::radio::parse_pls(s);
        let _ = fuga::source::radio::parse_m3u(s);
    }
});

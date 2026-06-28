#![no_main]
//! Fuzz the yt-dlp `--dump-json` line parser (untrusted subprocess output).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = fuga::source::youtube::parse_search_record(s);
    }
});

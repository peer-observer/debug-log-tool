#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| drnlog_fuzz::line_parse(data));

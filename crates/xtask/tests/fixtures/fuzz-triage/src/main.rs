//! Deliberate crashes for the isolated xtask triage integration test.
#![no_main]

use libfuzzer_sys::{fuzz_mutator, fuzz_target};

fuzz_target!(|data: &[u8]| {
    if std::env::var("FUZZ_TRIAGE_FIXTURE_MODE").as_deref() == Ok("fixed") {
        return;
    }
    let message = match data.first() {
        Some(b'A') => "triage fixture primary failure",
        Some(b'B') => "triage fixture secondary failure",
        _ => return,
    };
    // Both failures share a stack location, but have distinct failure identities.
    panic!("{message}");
});

fuzz_mutator!(
    |data: &mut [u8], _size: usize, max_size: usize, _seed: u32| {
        if max_size == 0 {
            return 0;
        }
        data[0] = if std::env::var("FUZZ_TRIAGE_FIXTURE_MODE").as_deref() == Ok("changed") {
            b'B'
        } else {
            b'A'
        };
        1
    }
);

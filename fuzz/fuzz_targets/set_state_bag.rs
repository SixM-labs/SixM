#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = sixm_net::fuzzing::decode_set_state_bag(data);
});

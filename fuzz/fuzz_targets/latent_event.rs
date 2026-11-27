#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = sixm_net::fuzzing::decode_latent_body(data);
    let _ = sixm_net::fuzzing::parse_latent_stream_header(data);
});

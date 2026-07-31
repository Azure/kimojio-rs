#![no_main]
//! Drives the decoder for systemd-resolved Varlink replies with arbitrary wire
//! bytes.
//!
//! The raw bytes include the NUL frame terminator. One entry point therefore
//! tests the framing, the reply size limit, the UTF-8 limits, the streaming
//! JSON scan, and the validation of the address fields.

use kimojio::resolver::fuzz_decode_varlink_reply;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|wire: &[u8]| {
    fuzz_decode_varlink_reply(wire);
});

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
fn main() {
    let io_uring = std::env::var("CARGO_FEATURE_IO_URING").is_ok();
    let epoll = std::env::var("CARGO_FEATURE_EPOLL").is_ok();
    let count = [io_uring, epoll].iter().filter(|&&b| b).count();
    if count > 1 {
        panic!("Only one I/O backend feature may be enabled at a time (io_uring, epoll)");
    }
    println!("cargo:rerun-if-changed=build.rs");
}

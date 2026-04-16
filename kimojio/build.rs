// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Build script for kimojio.
//!
//! Validates that exactly one I/O backend feature is selected.  When no
//! backend is selected a warning is emitted rather than a hard error so that
//! `cargo check --no-default-features` (without specifying a backend) does not
//! fail outright in CI environments that check feature flag hygiene.

fn main() {
    let io_uring = std::env::var("CARGO_FEATURE_IO_URING").is_ok();
    let epoll = std::env::var("CARGO_FEATURE_EPOLL").is_ok();
    let iocp = std::env::var("CARGO_FEATURE_WINDOWS_IOCP").is_ok();

    let count = [io_uring, epoll, iocp].iter().filter(|&&b| b).count();
    if count == 0 {
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if target_os == "linux" {
            eprintln!(
                "cargo:warning=No I/O backend feature selected. \
                 Enable 'io_uring' (default) or 'epoll' for the Linux backend."
            );
        } else if target_os == "windows" {
            eprintln!(
                "cargo:warning=No I/O backend feature selected on Windows. \
                 Enable 'windows-iocp'."
            );
        }
    } else if count > 1 {
        panic!(
            "Only one I/O backend feature may be enabled at a time \
             (io_uring, epoll, windows-iocp). Found {} enabled.",
            count
        );
    }

    println!("cargo:rerun-if-changed=build.rs");
}

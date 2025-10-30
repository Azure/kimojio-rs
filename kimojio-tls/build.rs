// Copyright (c) Microsoft Corporation. All rights reserved.
fn main() {
    cc::Build::new()
        .file("src/openssl_stream.c")
        .compile("openssl_stream");

    println!("cargo:rustc-link-lib=dylib=ssl");
    println!("cargo:rustc-link-lib=dylib=crypto");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/openssl_stream.c");
}

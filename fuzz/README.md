# Fuzz targets

Coverage-guided fuzz tests for the protocol decoders of Kimojio. This crate is
outside of the parent workspace. Therefore the sanitizer flags and the nightly
flags that the fuzz profile needs do not change a usual build of the runtime.

```sh
cd fuzz
cargo +nightly fuzz build                     # build all targets
cargo +nightly fuzz run <target>              # run one target until interrupted
cargo +nightly fuzz run <target> -- -max_total_time=900 -workers=6 -jobs=6
```

The fuzzer writes each failure case to `fuzz/artifacts/<target>/`. To run a
failure case again, use
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<case>`.

Add a target together with the code that it tests. Each target documents the
interface that it tests and the meaning of a failure in that target.

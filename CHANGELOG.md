# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.15.0] - 2026-03-19

### Added
- `HandleTable::drain` for bulk handle removal ([#29])

### Fixed
- `SenderUnbounded::send` now returns `Err(message)` after the channel is
  closed, matching the bounded `Sender` behavior ([#34])
- Test temp files moved from `/tmp/` root to `/tmp/kimojio-test/` ([#34])

### Changed
- Bump `intrusive-collections` to 0.10 ([#30])

### Removed
- `MAX_TASK_STACK_SIZE` assert from `block_on` ([#32])

## [0.14.1] - 2026-02-18

### Added
- File copy example demonstrating async I/O with polled operations ([#20])
- Doc comments for methods that were missing them ([#22])

### Fixed
- Correct `in_flight` counter for iopoll tracing ([#18])
- Completions accounting in `process_completions` ([#26])

### Changed
- Add on-by-default `tls` feature flag ([#17])
- Implement `Send` for `TlsServerContext` ([#14])
- Upgrade Rust toolchain to 1.92 ([#21])
- Bump `time` from 0.3.43 to 0.3.47 ([#24])

## [0.14.0] - 2025-11-19

### Added
- `#[kimojio::test]` and `#[kimojio::main]` proc macros ([#11])
- TLS echo server example ([#6])

### Fixed
- docs.rs link ([#5])

### Changed
- `RingFuture` lifetime parameter improvements ([#10])
- Mark `pointer_from_buffer` as `unsafe` ([#9])

## [0.13.2] - 2025-10-30

- Initial open-source release.

[0.15.0]: https://github.com/Azure/kimojio-rs/compare/v0.14.0...HEAD
[0.14.1]: https://github.com/Azure/kimojio-rs/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/Azure/kimojio-rs/compare/v0.13.2...v0.14.0
[0.13.2]: https://github.com/Azure/kimojio-rs/releases/tag/v0.13.2

[#5]: https://github.com/Azure/kimojio-rs/pull/5
[#6]: https://github.com/Azure/kimojio-rs/pull/6
[#9]: https://github.com/Azure/kimojio-rs/pull/9
[#10]: https://github.com/Azure/kimojio-rs/pull/10
[#11]: https://github.com/Azure/kimojio-rs/pull/11
[#14]: https://github.com/Azure/kimojio-rs/pull/14
[#17]: https://github.com/Azure/kimojio-rs/pull/17
[#18]: https://github.com/Azure/kimojio-rs/pull/18
[#20]: https://github.com/Azure/kimojio-rs/pull/20
[#21]: https://github.com/Azure/kimojio-rs/pull/21
[#22]: https://github.com/Azure/kimojio-rs/pull/22
[#24]: https://github.com/Azure/kimojio-rs/pull/24
[#26]: https://github.com/Azure/kimojio-rs/pull/26
[#29]: https://github.com/Azure/kimojio-rs/pull/29
[#30]: https://github.com/Azure/kimojio-rs/pull/30
[#32]: https://github.com/Azure/kimojio-rs/pull/32
[#34]: https://github.com/Azure/kimojio-rs/pull/34

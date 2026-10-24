# Contributing

Thanks for helping with IronAV.

## Licensing of contributions

IronAV is dual-licensed (AGPL-3.0-or-later and a commercial license; see `COMMERCIAL-LICENSE.md`). For that to work,
Nervosys must be able to relicense every contribution. By submitting a pull request you confirm that:

- you wrote the contribution (or have the right to submit it), and
- you license it to Nervosys under the AGPL-3.0-or-later **and** grant Nervosys a perpetual, worldwide,
  non-exclusive, royalty-free right to sublicense and distribute it under other terms, including commercial licenses.

Sign off each commit (`git commit -s`) to record this. A formal contributor license agreement may replace this
sign-off later; maintainers will tell you if so.

Do not submit code copied from FFmpeg, x264 or other projects whose licenses conflict with dual licensing.
IronAV is an independent implementation.

## Development

- `cargo build` / `cargo test` must pass.
- New formats, codecs and filters must be added to their registries, which feed the ontology automatically.
  `ironav-engine/tests/e2e.rs` checks the ontology stays consistent with the registries.
- Errors should use a stable `ErrorCode` and, where possible, a `hint` and `alternatives` so agents can recover.

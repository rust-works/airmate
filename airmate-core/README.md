# airmate-core

The sans-I/O translation core of [airmate](https://github.com/rust-works/airmate).

It has no `tokio`, no HTTP client, no clock, no randomness and no filesystem. Every function is
deterministic given its inputs, so every behaviour is unit-testable and fuzzable. See the
specification in [issue #3](https://github.com/rust-works/airmate/issues/3).

//! Shared test support for `clawde-api` integration tests.
//!
//! Files under `tests/common/` are not compiled as standalone test binaries;
//! test files import them with `mod common;`. Each test binary compiles this
//! module separately and uses a subset of the helpers, so unused items in any
//! one binary are expected.
#![allow(dead_code)]

pub mod mock_provider;

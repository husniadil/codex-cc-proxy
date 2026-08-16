//! Translation between the Anthropic Messages API and the OpenAI Responses API.
//!
//! This crate is pure: no sockets, no clock, no filesystem, no configuration
//! policy. Every rule in `docs/proxy-behavior.md` §2 and §5 is a function over
//! data, which is what makes the rules testable without a backend.

#![forbid(unsafe_code)]

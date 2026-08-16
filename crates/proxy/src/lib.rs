//! The daemon: ingress, transports, sessions, and credentials.
//!
//! Exposed as a library so the suite can drive the real surfaces rather than a
//! stand-in for them.

#![forbid(unsafe_code)]

pub mod auth;
pub mod catalog;
pub mod config;
pub mod control;
pub mod daemon;
pub mod error;
pub mod estimate;
pub mod ingress;
pub mod recorder;
pub mod render;
pub mod session;
pub mod upstream;

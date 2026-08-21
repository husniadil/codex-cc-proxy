//! `docs/api.md` §3 — the control socket's JSON-RPC vocabulary.
//!
//! The method names are semver-bound. A shipped name is never repurposed or
//! removed within a major version; only new ones are added.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const VERSION: &str = "2.0";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: VERSION.to_owned(),
            id: Value::from(id),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failed(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: VERSION.to_owned(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// JSON-RPC's own codes, used with their standard meanings so a generic client
/// can interpret them without knowing this daemon.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Application errors start here, per the specification's reservation.
    pub const APPLICATION_ERROR: i32 = -32000;
}

/// Every method the socket answers.
///
/// Listed as a constant rather than left implicit so the CLI, the tests, and
/// any future front-end all agree on the surface, and so removing one is a
/// visible change.
pub const METHODS: [&str; 19] = [
    "status",
    "shutdown",
    "login",
    "login.cancel",
    "accounts",
    "accounts.select",
    "accounts.rename",
    "accounts.forget",
    "models",
    "tiers",
    "tiers.set",
    "effort.set",
    "cross_account_tiers.set",
    "usage",
    "usage.refresh",
    "env",
    "doctor",
    "record.start",
    "record.stop",
];

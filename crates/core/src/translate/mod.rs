//! `docs/proxy-behavior.md` §2 and §5 — translation in both directions.

mod request;

pub use request::TranslateOptions;
pub use request::discovered_tool_names;
pub use request::translate_request;

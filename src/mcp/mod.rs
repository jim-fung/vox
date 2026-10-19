//! MCP server module.
//!
//! Split into `protocol.rs` (JSON-RPC framing + dispatch) and `tools.rs` (the 14
//! tool implementations) for navigability — see review R4.

mod protocol;
mod tools;

pub use protocol::run_server;

//! Cursor provider for rpi: the AgentService streaming protocol with the host's tools bridged over
//! MCP (DESIGN §4.9).
//!
//! `rpi-cursor-lite` (the image's old cursor package) can only chat: its JS worker keeps the
//! request text-only, so a Cursor model reached through it can never call a tool. This crate
//! replaces it with a streaming provider that
//!
//! 1. advertises the host's tools as Cursor MCP tools (`x-cursor-agent-allowed-tools:
//!    mcp_tool_call` also removes Cursor's own built-in tool registry, so the model has nothing to
//!    call but our tools),
//! 2. translates Cursor's `InteractionUpdate`/`ExecServerMessage` stream into rpi's
//!    `AssistantMessageEvent`s, answering the framing handshakes (`requestContext`, KV blobs) on
//!    the way, and
//! 3. ends the turn at the first `mcpArgs` with a `toolUse` stop reason, so rpi executes the call
//!    and asks again with the tool result in the history. Cursor's per-turn history is rebuilt from
//!    that history every request (the whole conversation is replayed as blobs), so nothing has to
//!    stay alive between turns.
//!
//! Credentials never leave this process: the API key arrives from rpi (`opts.api_key`, resolved
//! from the account's auth store) or `CURSOR_API_KEY`, is exchanged for a short-lived bearer token,
//! and is never logged.
// The exported entry point takes raw pointers from the host (the SDK's `export_provider!`); the
// macro's own body is what dereferences them, under the same guarantee `zephyr-policy` relies on.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod auth;
pub mod history;
pub mod models;
pub mod provider;
pub mod transport;
pub mod wire;

use rpi_ai::{Model, Provider, SimpleStreamOptions};
use rpi_plugin_sdk as sdk;
use std::sync::Arc;

/// Provider id; `[[account]] provider = "cursor"` and `zephyr login cursor` both name it.
pub const ID: &str = "cursor";
/// Catalog/base-URL tag rpi records for the provider. Requests do not use it (the agent endpoint is
/// `wire::endpoint()`), but every model needs one.
pub const BASE_URL: &str = "https://api2.cursor.sh";

fn factory(_: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
    Arc::new(provider::Cursor)
}

/// Declares this crate's catalog: rpi 0.14 takes each provider's models from the manifest
/// (`ProviderManifest { id, models, apiKeyEnv, requiresAuth }`, `deny_unknown_fields`, and every
/// model's `provider` must equal `id`), and serves them with the streaming fn registered beside it.
fn manifest() -> serde_json::Value {
    serde_json::json!({
        "id": ID,
        // `rpi auth login --provider cursor` stores the key here, and rpi resolves it into
        // `opts.api_key`; the env var is the door for a key supplied by the container instead.
        "apiKeyEnv": "CURSOR_API_KEY",
        "models": models::catalog(),
    })
}

sdk::export_provider!(ID, BASE_URL, "openai-completions", manifest(), factory);

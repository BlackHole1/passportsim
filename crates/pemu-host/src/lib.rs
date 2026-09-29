//! Native host: daemon and instance pool, MCP (stdio and streamable HTTP), HTTP and WebSocket,
//! USB Serial/JTAG endpoints, pty (macOS only), WISP relay, artifacts, boot cache and native asset
//! overrides. It also holds the host-specific layers every other crate goes through: directory
//! roles (`paths`) and platform services (`platform`).

pub mod artifacts;
pub mod assets;
pub mod attach;
pub mod audio_root;
pub mod auth;
pub mod backend;
pub mod boot_cache;
pub mod build_dir;
pub mod build_id;
pub mod daemon;
pub mod device;
pub mod endpoints;
pub mod hooks;
pub mod host_file;
pub mod http;
pub mod hub;
pub mod mcp_http;
pub mod mcp_stdio;
pub mod pacing;
pub mod paths;
pub mod platform;
pub mod png;
pub mod pool;
pub mod relay_wisp;
pub mod wav;
pub mod webui;
pub mod ws;

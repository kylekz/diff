//! WSL host-process RPC: wire protocol + client for talking to `dv-host`
//! over a `wsl.exe --exec` stdio channel — see
//! docs/phase-5-implementation-plan.md and docs/phase-5-wsl.md.
//!
//! S1 shipped handshake + `proc/exec` (real `wsl.exe` spawn + a test seam
//! for any process speaking the protocol). S2 adds `manager.rs` (the
//! per-distro `HostEntry` registry, consulted by
//! [`crate::command::CommandBuilder::new`]) and `blob/get`. `install.rs`
//! (sidecar install/upgrade) lands in S3.

pub mod client;
pub mod manager;
pub mod proto;

pub use client::{ExecOutcome, HostClient, RequestFailure};
pub use manager::enable_hosts;
pub use proto::{BlobGetParams, BlobGetResult, Hello, Notification, PROTO_VERSION, RpcError};

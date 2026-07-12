//! WSL host-process RPC: wire protocol + client for talking to `dv-host`
//! over a `wsl.exe --exec` stdio channel — see
//! docs/phase-5-implementation-plan.md and docs/phase-5-wsl.md.
//!
//! S1 scope only (handshake + `proc/exec`, real `wsl.exe` spawn + a test
//! seam for any process speaking the protocol). `manager.rs` (per-distro
//! registry) and `install.rs` (sidecar install/upgrade) land in S2/S3 —
//! nothing in dv-core wires this module in yet; it is not called from
//! anywhere else in the crate.

pub mod client;
pub mod proto;

pub use client::{ExecOutcome, HostClient};
pub use proto::{Hello, Notification, PROTO_VERSION, RpcError};

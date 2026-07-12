//! WSL host-process RPC: wire protocol + client for talking to `dv-host`
//! over a `wsl.exe --exec` stdio channel — see
//! docs/phase-5-implementation-plan.md and docs/phase-5-wsl.md.
//!
//! S1 shipped handshake + `proc/exec` (real `wsl.exe` spawn + a test seam
//! for any process speaking the protocol). S2 added `manager.rs` (the
//! per-distro `HostEntry` registry, consulted by
//! [`crate::command::CommandBuilder::new`]) and `blob/get`. S3 adds
//! `install.rs` (sidecar resolution + stream-install/upgrade), wired into
//! `manager::client_for` as the default path when `DV_HOST_PATH` isn't set.

pub mod client;
pub mod install;
pub mod manager;
pub mod proto;

pub use client::{ExecOutcome, HostClient, RequestFailure};
pub use install::{HostBinary, HostBinarySource, InstallError};
pub use manager::enable_hosts;
pub use proto::{BlobGetParams, BlobGetResult, Hello, Notification, PROTO_VERSION, RpcError};

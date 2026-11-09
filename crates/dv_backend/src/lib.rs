//! The remote side of dv's `Backend` interface (docs/PLAN.md D6, §5): the JSON-lines stdio
//! protocol, the server loop behind `dv backend --stdio`, and the `RemoteBackend` client. The
//! trait itself and the in-process `LocalBackend` live in `dv_core::backend`, which the diff
//! pipeline and the review store are written against (PLAN §5.1).
//!
//! - [`proto`]: the wire format, one JSON object per line, bytes in base64.
//! - [`server`]: [`server::serve`], a `LocalBackend` per open repository answering requests
//!   concurrently, which `dv backend --stdio` runs on its stdin and stdout.
//! - [`client`]: a [`Connection`] to one server at a time (one per WSL distro, or a named
//!   loopback), started lazily and restarted after a death (PLAN §5.2), [`RemoteBackend`], one
//!   worktree's `Backend` through it, and [`Connections`], the app's registry of them.
//! - [`launch`]: how a connection's server starts ([`Launcher`]): on a thread, as a child
//!   process such as a loopback `dv backend --stdio`, or inside a WSL distro through `wsl.exe`.
//! - [`wsl`]: the distros, the scripts dv runs inside one, and the start of its server;
//!   [`install`]: this app's Linux builds and their install in a distro (PLAN §5.2, §13.2).
//! - [`build`]: build hashes, which `hello` compares (PLAN §5.2).
//!
//! Every child it starts (servers, `wsl.exe`) dies with this process, however it ends
//! (`dv_core::job`), and gets no console window on Windows (`toolbox_process`).
//!
//! gpui: **never**. The Linux build of this crate is the WSL backend, a static musl binary
//! without a window system; `script/check` fails when gpui appears in its dependency tree
//! (PLAN §4, §13.2).

pub mod build;
pub mod client;
pub mod install;
pub mod launch;
pub mod proto;
pub mod server;
pub mod wsl;

pub use client::{
    Connection, ConnectionKey, ConnectionState, Connections, RemoteBackend, Subscription,
};
pub use launch::{Asked, Host, LaunchError, Launcher, StartOptions, Transport};

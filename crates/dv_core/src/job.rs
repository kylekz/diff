//! Child processes that die with this one (docs/PLAN.md §5.2, §10.4, §18.3).
//!
//! A server's `wsl.exe` ends when the app closes its stdin, and the CLI's run inside a distro
//! when the command does. But a process that is killed (a tool's timeout, Task Manager, a
//! `taskkill` without `/T`) closes nothing in order, and a child left behind would run on:
//! the CLI's `wsl.exe` and the distro's `dv review wait` behind it until their round ends.
//! So on Windows every such child joins a kill-on-close job object this process holds open
//! and never closes (`toolbox_process::tie_to_this_process`): when this process ends, however
//! it ends, the OS closes the job and kills what is in it, and killing `wsl.exe` ends what it
//! runs inside the distro. Ctrl-C reaches the child through the console it shares, as before.
//! Elsewhere there is nothing to do: the Linux build starts no `wsl.exe`.
//!
//! A process started through `toolbox_process` (`Backend::spawn_stdio`, git, the setup checks)
//! needs none of this: it runs in a job of its own, which dies with this process too.

use std::process::Child;

/// Puts `child` in this process's kill-on-close job (module docs), best effort: when that
/// fails it is logged and the child relies on its stdin closing.
pub fn tie_to_this_process(child: &Child) {
    if let Err(error) = toolbox_process::tie_to_this_process(child) {
        log::warn!(
            "couldn't tie process {} to dv's job object ({error}); it relies on its stdin closing",
            child.id()
        );
    }
}

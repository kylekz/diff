//! How a GitHub name is spelled where dv keeps things by it (docs/PLAN.md §9.3, §9.4): a
//! host, an owner, a repository or a login as one component of a store path
//! (`github/<host>/<owner>/<repo>/pr-<n>/<login>.json`) or of a ref
//! (`refs/dv/pr/<owner>/<repo>/<n>/head`).
//!
//! One rule for both, so a name is the same component everywhere: lowercase, since GitHub's
//! names have no case, then `%xx` (the byte in hex) for everything that isn't safe both as a
//! component of a git ref and as a file name on Windows:
//!
//! - any character outside `[a-z0-9._-]`: a host's `:`, a bot login's brackets, and `%` itself;
//! - a leading `.` (`.github`), which git refuses and the store reads as a hidden file;
//! - a `.` after a `.`, which git refuses, and a trailing one, which Windows drops;
//! - the `.` of a final `.lock`, which git refuses;
//! - the first letter of a name Windows keeps for a device (`con`, `nul`, `com1`, with or
//!   without an extension).
//!
//! Only `%` starts an escape and `%` is itself escaped, so two names that differ by more than
//! case never share a spelling.

/// The names Windows keeps for devices, whatever follows a first `.`.
const DEVICES: &[&str] = &[
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
    "com8", "com9", "lpt0", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// `name` as one component of a store path or of a ref (module docs).
pub fn spell(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    let bytes = name.as_bytes();
    let lock = name.strip_suffix(".lock").map(str::len);
    let device = DEVICES.contains(&name.split('.').next().unwrap_or(&name));
    let mut spelled = String::with_capacity(name.len());
    for (index, &byte) in bytes.iter().enumerate() {
        let plain = match byte {
            b'.' => {
                index > 0
                    && index + 1 < bytes.len()
                    && bytes[index - 1] != b'.'
                    && lock != Some(index)
            }
            b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => !(device && index == 0),
            _ => false,
        };
        if plain {
            spelled.push(char::from(byte));
        } else {
            spelled.push_str(&format!("%{byte:02x}"));
        }
    }
    spelled
}

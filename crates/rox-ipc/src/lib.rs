//! The control socket (ADR 22): rox's one machine interface, newline-delimited
//! JSON-RPC over a Unix domain socket. Everything external goes through this
//! surface, with MPRIS staying the standard desktop shim in front of it.
//!
//! This crate owns the wire: the frame and error types, the listener with the
//! same staging-and-rename bind discipline as the single-instance guard, the
//! per-connection threads that parse frames and hold the version handshake,
//! the [`Events`] registry that pushes id-less frames to connections that
//! subscribed, and a small blocking client for the CLI and tests. What it
//! never owns is an answer: every method past `hello` and `subscribe` crosses
//! to the app as a [`Request`] on an async channel and comes back through the
//! request's responder, so the app side stays the single place that touches
//! the player and the library. Events flow the other way through the same
//! division: the app decides what happened and emits, the crate carries it.
//!
//! Unix speaks std's domain sockets; Windows speaks named pipes through
//! interprocess, behind the same frame discipline and the same generic
//! connection loop, so the two backends can't drift on the protocol.
//!
//! The Windows half of the single-instance guard lives here too, in
//! [`instance`]. It has nothing to do with the protocol, but it's the same
//! transport and the same pipe naming, and this is the crate that can be
//! cross-checked for Windows without dragging the app's native build along.

mod events;
mod protocol;
mod server;

pub mod client;
#[cfg(windows)]
pub mod instance;

pub use events::Events;
pub use protocol::{PROTOCOL_VERSION, RpcError};
pub use server::{Cleanup, Request, Responder, Server};

use std::path::{Path, PathBuf};

/// Where the control socket lives for a data directory. Keyed to the data
/// dir the same way the single-instance guard's socket is, so a `--portable`
/// or `--fresh` run gets its own control surface instead of steering the
/// daily driver. Sockets belong in the runtime dir; the data dir stands in
/// only where there is none.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    let hash = data_dir_hash(data_dir);
    if cfg!(windows) {
        // Named pipes are in their own flat namespace, not the
        // filesystem; this is the canonical spelling of ours, and the
        // backends peel the prefix back off to name the pipe.
        return PathBuf::from(format!(r"\\.\pipe\rox-ipc-{hash:016x}"));
    }
    socket_in(data_dir, &format!("rox-ipc-{hash:016x}.sock"))
}

/// Where a per-data-dir socket file goes: the runtime dir, else beside the
/// data dir, else the user's cache dir when the data dir's path is too long
/// to bind. Never a shared temp dir, where another user could squat the
/// name.
pub fn socket_in(data_dir: &Path, name: &str) -> PathBuf {
    choose_socket_dir(dirs::runtime_dir(), data_dir, dirs::cache_dir(), name)
}

fn choose_socket_dir(
    runtime: Option<PathBuf>,
    data_dir: &Path,
    cache: Option<PathBuf>,
    name: &str,
) -> PathBuf {
    if let Some(dir) = runtime {
        return dir.join(name);
    }

    let beside = data_dir.join(name);
    if fits(&beside) {
        return beside;
    }

    cache
        .map(|dir| dir.join("rox").join(name))
        .filter(|path| fits(path))
        .unwrap_or(beside)
}

/// `sun_path` is 108 bytes on Linux and 104 on macOS and the BSDs, NUL
/// included.
const SUN_PATH: usize = if cfg!(target_os = "linux") { 108 } else { 104 };

/// Both binds stage under `<name>.<pid>.sock` first: the dot, a pid of up to
/// seven digits, and the NUL.
const STAGING_HEADROOM: usize = 9;

fn fits(path: &Path) -> bool {
    path.as_os_str().len() + STAGING_HEADROOM <= SUN_PATH
}

/// Creates the socket's folder if it's missing, user-only, since the cache
/// dir fallback may not exist yet.
#[cfg(unix)]
pub fn ensure_socket_dir(path: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;

    if let Some(dir) = path.parent() {
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
}

/// The key every per-data-dir name hangs off: the control socket here, the
/// single-instance pipe in [`instance`]. It only has to agree with itself
/// across two runs of the same binary, which is well inside what
/// `DefaultHasher` guarantees.
fn data_dir_hash(data_dir: &Path) -> u64 {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data_dir.hash(&mut hasher);
    hasher.finish()
}

/// A socket path as interprocess requires it: the bare pipe name in the
/// named-pipe namespace. Accepts the canonical `\\.\pipe\` spelling from
/// [`socket_path`] and a bare name alike, so a hand-typed `--socket` works
/// either way.
#[cfg(windows)]
pub(crate) fn pipe_name(path: &Path) -> std::io::Result<interprocess::local_socket::Name<'static>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName as _};

    let text = path.to_string_lossy();
    let bare = text.strip_prefix(r"\\.\pipe\").unwrap_or(&text).to_owned();
    bare.to_ns_name::<GenericNamespaced>()
}

/// Whether a pipe create failed because the name is already served. The
/// listener's first instance carries `FILE_FLAG_FIRST_PIPE_INSTANCE`, and
/// Windows answers a second process asking for the same name with
/// `ERROR_ACCESS_DENIED`, which std reads as a permission problem rather
/// than an address in use. `ERROR_PIPE_BUSY` is what a pipe that someone
/// created with an instance limit answers instead.
#[cfg(windows)]
pub(crate) fn pipe_taken(err: &std::io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_PIPE_BUSY: i32 = 231;

    err.kind() == std::io::ErrorKind::AddrInUse
        || matches!(
            err.raw_os_error(),
            Some(ERROR_ACCESS_DENIED | ERROR_PIPE_BUSY)
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "rox-ipc-0123456789abcdef.sock";

    #[test]
    fn the_runtime_dir_wins_whatever_the_data_dir() {
        let path = choose_socket_dir(
            Some("/run/user/1000".into()),
            Path::new("/home/me/.local/share/rox"),
            Some("/home/me/.cache".into()),
            NAME,
        );
        assert_eq!(path, Path::new("/run/user/1000").join(NAME));
    }

    #[test]
    fn a_short_data_dir_keeps_its_socket() {
        let data_dir = Path::new("/Users/me/Library/Application Support/rox");
        let path = choose_socket_dir(
            None,
            data_dir,
            Some("/Users/me/Library/Caches".into()),
            NAME,
        );
        assert_eq!(path, data_dir.join(NAME));
    }

    #[test]
    fn a_data_dir_too_deep_to_bind_moves_to_the_cache_dir() {
        let deep = format!("/private/tmp/{}/rox-data", "x".repeat(80));
        let path = choose_socket_dir(
            None,
            Path::new(&deep),
            Some("/Users/me/Library/Caches".into()),
            NAME,
        );

        assert_eq!(path, Path::new("/Users/me/Library/Caches/rox").join(NAME));
        assert!(fits(&path));
    }

    #[test]
    fn with_nowhere_shorter_the_data_dir_stays() {
        let deep = PathBuf::from(format!("/private/tmp/{}/rox-data", "x".repeat(80)));
        assert_eq!(choose_socket_dir(None, &deep, None, NAME), deep.join(NAME));
    }
}

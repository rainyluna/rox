//! How this process was installed, as far as the sandbox or the launcher
//! tells us: bare, inside a Flatpak, or out of an AppImage. Bare covers a
//! distro package, the tarball, nix, and a dev build, and it's the answer
//! everything above was written against. The other two are the channels
//! where the executable's own folder stops meaning what it usually means:
//! a Flatpak's `/app/bin` is unreachable from the host, and an AppImage
//! runs from a squashfs mount under `/tmp` that's read-only and gone after
//! exit. The settings layer, the updater, and the MCP page each ask here
//! before they trust `current_exe()`.
//!
//! Detection is decided once per process. The environment variables it
//! reads are set by the launcher before `main` and never change, and a
//! caller that got a different answer mid-run would split the stores the
//! same way a mid-run portable flip would. Nothing here draws, and
//! nothing here depends on the app above it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// How this process was installed, as far as the sandbox or the launcher
/// tells us. Bare covers a distro package, the tarball, nix, and dev builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bare,
    Flatpak,
    AppImage,
}

/// The reverse-DNS id the Flatpak and the AppImage menu entry use.
pub const APP_ID_RDNS: &str = "com.zealsprince.rox";

/// Decided once per process. Flatpak: `FLATPAK_ID` in the environment or
/// `/.flatpak-info` on disk. AppImage: `APPIMAGE` and `APPDIR` both set and
/// the running executable under `APPDIR`.
pub fn kind() -> Kind {
    static KIND: OnceLock<Kind> = OnceLock::new();

    *KIND.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_default();
        let flatpak_info = Path::new("/.flatpak-info").exists();
        // A closure rather than `var_os` itself: the fn item is generic over
        // its key and doesn't satisfy `Fn(&str)` for every lifetime.
        detect(|name: &str| std::env::var_os(name), flatpak_info, &exe)
    })
}

/// The detection proper, over the inputs rather than the process, so the
/// tests can hand it an environment. `flatpak_info` is whether
/// `/.flatpak-info` exists, the one signal that isn't an environment
/// variable.
fn detect(env: impl Fn(&str) -> Option<OsString>, flatpak_info: bool, exe: &Path) -> Kind {
    if flatpak_info || env("FLATPAK_ID").is_some() {
        return Kind::Flatpak;
    }

    // Both variables and the executable inside the mount. A shell opened
    // from an AppImage inherits APPIMAGE and APPDIR, and a rox started from
    // that shell must not call itself one.
    let (Some(_), Some(appdir)) = (env("APPIMAGE"), env("APPDIR")) else {
        return Kind::Bare;
    };
    if exe.starts_with(PathBuf::from(appdir)) {
        return Kind::AppImage;
    }

    Kind::Bare
}

/// The .AppImage file this process was launched from, when `kind()` is
/// AppImage. Stable across launches, unlike `current_exe()`, which is the
/// squashfs mount and changes its random suffix every run.
pub fn appimage() -> Option<&'static Path> {
    static APPIMAGE: OnceLock<Option<PathBuf>> = OnceLock::new();

    APPIMAGE
        .get_or_init(|| {
            if kind() != Kind::AppImage {
                return None;
            }
            std::env::var_os("APPIMAGE").map(PathBuf::from)
        })
        .as_deref()
}

/// Whether a path came back through the document portal
/// (`$XDG_RUNTIME_DIR/doc/...`) rather than as a real host path. Only ever
/// true inside a Flatpak that lacks access to the folder that was picked.
pub fn is_portal_path(path: &Path) -> bool {
    dirs::runtime_dir().is_some_and(|dir| path.starts_with(dir.join("doc")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An environment built from a list of pairs, handed to `detect` in
    /// place of the process's own.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn nothing_set_is_bare() {
        assert_eq!(
            detect(env(&[]), false, Path::new("/usr/bin/rox")),
            Kind::Bare
        );
    }

    #[test]
    fn flatpak_by_variable() {
        let e = env(&[("FLATPAK_ID", "com.zealsprince.rox")]);
        assert_eq!(detect(e, false, Path::new("/app/bin/rox")), Kind::Flatpak);
    }

    #[test]
    fn flatpak_by_info_file() {
        assert_eq!(
            detect(env(&[]), true, Path::new("/app/bin/rox")),
            Kind::Flatpak
        );
    }

    /// The sandbox wins over the AppImage variables: a Flatpak can't be
    /// running out of a squashfs mount, so anything else is inherited noise.
    #[test]
    fn flatpak_outranks_appimage_variables() {
        let e = env(&[
            ("FLATPAK_ID", "com.zealsprince.rox"),
            ("APPIMAGE", "/home/me/rox.AppImage"),
            ("APPDIR", "/tmp/.mount_rox1234"),
        ]);
        assert_eq!(
            detect(e, false, Path::new("/tmp/.mount_rox1234/usr/bin/rox")),
            Kind::Flatpak
        );
    }

    #[test]
    fn appimage_with_exe_inside_the_mount() {
        let e = env(&[
            ("APPIMAGE", "/home/me/rox.AppImage"),
            ("APPDIR", "/tmp/.mount_rox1234"),
        ]);
        assert_eq!(
            detect(e, false, Path::new("/tmp/.mount_rox1234/usr/bin/rox")),
            Kind::AppImage
        );
    }

    /// The inherited case: a rox launched from a shell that an AppImage
    /// opened carries both variables but runs from somewhere else.
    #[test]
    fn appimage_variables_without_the_exe_inside_are_bare() {
        let e = env(&[
            ("APPIMAGE", "/home/me/rox.AppImage"),
            ("APPDIR", "/tmp/.mount_rox1234"),
        ]);
        assert_eq!(detect(e, false, Path::new("/usr/bin/rox")), Kind::Bare);
    }

    /// One variable without the other is not an AppImage launch.
    #[test]
    fn one_appimage_variable_alone_is_bare() {
        let e = env(&[("APPDIR", "/tmp/.mount_rox1234")]);
        assert_eq!(
            detect(e, false, Path::new("/tmp/.mount_rox1234/usr/bin/rox")),
            Kind::Bare
        );
    }

    /// The prefix check is by path component, so a sibling mount whose
    /// name merely extends APPDIR's doesn't match.
    #[test]
    fn appdir_prefix_is_by_component() {
        let e = env(&[
            ("APPIMAGE", "/home/me/rox.AppImage"),
            ("APPDIR", "/tmp/.mount_rox"),
        ]);
        assert_eq!(
            detect(e, false, Path::new("/tmp/.mount_rox1234/usr/bin/rox")),
            Kind::Bare
        );
    }
}

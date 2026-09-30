//! Where the host looks for a plugin's interpreter and programs: the plugin's
//! own `bin/`, then the directories from the user's search-path setting, then
//! PATH. The same list becomes the plugin process's PATH, so the programs a
//! plugin runs itself resolve the same way the Plugins page reported them.
//!
//! The Flatpak can't see the host's programs, so a plugin ships them in
//! `bin/`. A macOS app started from Finder gets a PATH without Homebrew, so
//! the user adds those directories to the setting.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

static EXTRA: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// Replaces the user's extra directories. rox calls this at startup and
/// whenever the setting changes.
pub fn set_extra_dirs(dirs: Vec<PathBuf>) {
    *EXTRA.write().unwrap_or_else(|e| e.into_inner()) = dirs;
}

pub fn extra_dirs() -> Vec<PathBuf> {
    EXTRA.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `<plugin>/bin`, the extra directories, then PATH, as one PATH-style value.
pub fn search_path(plugin_dir: Option<&Path>) -> OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    build(plugin_dir, &extra_dirs(), &path)
}

/// Split from [`search_path`] so tests never touch the process-wide setting.
pub fn build(plugin_dir: Option<&Path>, extras: &[PathBuf], path_var: &OsStr) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Some(dir) = plugin_dir {
        dirs.push(dir.join("bin"));
    }
    dirs.extend(extras.iter().cloned());
    dirs.extend(std::env::split_paths(path_var));

    // A directory holding the separator can't go in a PATH at all; drop it
    // rather than lose the whole list.
    dirs.retain(|dir| std::env::join_paths([dir]).is_ok());

    std::env::join_paths(dirs).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugin_bin_comes_first_then_extras_then_path() {
        let path = std::env::join_paths(["/usr/bin"]).unwrap();
        let joined = build(
            Some(Path::new("/plugins/tones")),
            &[PathBuf::from("/opt/extra")],
            &path,
        );
        let dirs: Vec<PathBuf> = std::env::split_paths(&joined).collect();

        assert_eq!(
            dirs,
            vec![
                Path::new("/plugins/tones").join("bin"),
                PathBuf::from("/opt/extra"),
                PathBuf::from("/usr/bin"),
            ]
        );
    }
}

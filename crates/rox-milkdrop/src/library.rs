//! What presets the engine has to choose from.
//!
//! A MilkDrop preset is a single `.milk` file, and a preset pack is a
//! directory of a few thousand of them, sometimes nested a few levels deep
//! with a `textures/` folder alongside. The scan picks those folders up
//! too, since projectM only searches the paths it's given. The user downloads packs themselves
//! (the contract names the three worth having) and drops them under
//! `milkdrop_dir()/presets`, so this is a plain filesystem walk over roots
//! rox never wrote and can't assume the shape of.
//!
//! The scan is deliberately dumb: find the files, sort them, hold the list.
//! It doesn't parse presets, doesn't validate them, and doesn't try to tell a
//! broken one from a working one, because the only thing that can answer that
//! is libprojectM's own parser at load time. A preset that fails comes back
//! as an `Event::PresetFailed` from the worker instead.
//!
//! The one bit of structure it does read back out is the directory layout,
//! because that's how the packs organise themselves. Cream of the Crop puts
//! its ~9800 presets in category folders like `Fractal` and `Dancer` one
//! level under the pack root, so [`PresetLibrary::folders`] hands those
//! folders to the panel and a [`Rotation`] narrows what the engine walks to
//! one of them.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

/// Which presets the rotation walks. Not which preset is on screen: an
/// explicit pick from the full list still loads, it just isn't what
/// Next, Previous and the timed switch move through.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Rotation {
    /// Everything the scan found.
    #[default]
    All,
    /// Only presets under this directory, at any depth below it.
    Folder(PathBuf),
    /// An explicit list of files: the user's favorites. Paths the library
    /// doesn't hold are skipped, so a favorite that was deleted or lives
    /// under a root this scan didn't walk drops out of the rotation
    /// rather than breaking it.
    Set(Vec<PathBuf>),
}

/// The presets found under a set of roots, plus the texture directories
/// projectM searches when a preset asks for an image.
/// `PartialEq` so a rescan can tell whether anything actually changed before
/// pushing a new list at the worker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresetLibrary {
    roots: Vec<PathBuf>,
    presets: Vec<PathBuf>,
    textures: Vec<PathBuf>,
}

impl PresetLibrary {
    /// Walk `roots` for `*.milk` (case-insensitive), sorted by path.
    ///
    /// Missing roots are skipped rather than reported: the default preset
    /// directory doesn't exist until the user puts something in it, and a
    /// blank panel with projectM's idle preset is the right answer for that,
    /// not an error. Symlinks aren't followed, so a root that points at its
    /// own parent doesn't hang the scan.
    ///
    /// Every `textures/` directory met under the roots joins the search
    /// list after `textures`, the app's own folder. projectM only ever looks
    /// in the paths it's handed, never beside the preset file, and the big
    /// packs ship their images inside the pack, so without this a pack
    /// dropped into the presets folder renders its textured presets blank.
    pub fn scan(roots: &[PathBuf], textures: Option<PathBuf>) -> PresetLibrary {
        let mut presets = Vec::new();
        let mut found_textures = Vec::new();
        for root in roots {
            for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
                let file_type = entry.file_type();
                if file_type.is_file() && is_preset(entry.path()) {
                    presets.push(entry.into_path());
                } else if file_type.is_dir() && is_textures_dir(entry.path()) {
                    found_textures.push(entry.into_path());
                }
            }
        }
        presets.sort();
        // Overlapping roots are the user's to make, and hitting the same file
        // twice in a shuffle would just feel like a bug.
        presets.dedup();
        found_textures.sort();
        found_textures.dedup();

        // The app's own folder goes first: projectM keeps the first file it
        // finds under a name, so a texture the user put there wins over a
        // pack's copy.
        let mut textures: Vec<PathBuf> = textures.into_iter().collect();
        for found in found_textures {
            if !textures.contains(&found) {
                textures.push(found);
            }
        }

        PresetLibrary {
            roots: roots.to_vec(),
            presets,
            textures,
        }
    }

    pub fn presets(&self) -> &[PathBuf] {
        &self.presets
    }

    /// Fold extra preset files into the scan. For favorites: the list is
    /// app-wide, a panel scans its own roots, and the backdrop only ever
    /// scans the default folder, so a favorite starred from a panel that
    /// points at another drive would otherwise be unreachable from
    /// everywhere else. Paths that aren't `.milk` files on disk are
    /// dropped, and what's already in the list stays where it is.
    pub fn extend(&mut self, extra: &[PathBuf]) {
        let missing: Vec<PathBuf> = extra
            .iter()
            .filter(|path| is_preset(path) && path.is_file() && self.index_of(path).is_none())
            .cloned()
            .collect();
        if missing.is_empty() {
            return;
        }
        self.presets.extend(missing);
        self.presets.sort();
        self.presets.dedup();
    }

    pub fn is_empty(&self) -> bool {
        self.presets.is_empty()
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The directories projectM searches for a preset's images, in the
    /// order it should search them.
    pub fn textures(&self) -> &[PathBuf] {
        &self.textures
    }

    /// Where `path` sits in the sorted list, so the worker can step forward
    /// and back from whatever's on screen.
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.presets
            .binary_search_by(|p| p.as_path().cmp(path))
            .ok()
    }

    /// Every directory that directly holds at least one preset, sorted,
    /// so the panel can offer a pack's own category folders as rotations.
    ///
    /// Directly is the whole point. A pack root that only contains category
    /// folders isn't in here, because picking it would just mean All under
    /// a different name, and a folder with a `textures/` subdirectory and no
    /// `.milk` of its own isn't a rotation anyone wants to sit in.
    pub fn folders(&self) -> Vec<PathBuf> {
        let mut folders: Vec<PathBuf> = self
            .presets
            .iter()
            .filter_map(|preset| preset.parent())
            .map(Path::to_path_buf)
            .collect();
        // The preset list is sorted by full path, so parents come out grouped
        // but not in parent order: `a/b/x.milk` sorts before `a/x.milk`.
        folders.sort();
        folders.dedup();
        folders
    }

    /// Indices into `presets()` that `rotation` selects. Empty means the
    /// rotation matched nothing, which the worker treats as All.
    ///
    /// Matching is by path component, never by string prefix, so a rotation
    /// on `Fractal` leaves `Fractal Extras` alone.
    pub fn rotation_indices(&self, rotation: &Rotation) -> Vec<usize> {
        match rotation {
            Rotation::All => (0..self.presets.len()).collect(),
            Rotation::Folder(folder) => self
                .presets
                .iter()
                .enumerate()
                .filter(|(_, preset)| preset.starts_with(folder))
                .map(|(index, _)| index)
                .collect(),
            Rotation::Set(paths) => {
                let mut indices: Vec<usize> = paths
                    .iter()
                    .filter_map(|path| self.index_of(path))
                    .collect();
                // Library order, once each: a favorite starred twice by two
                // panels racing isn't twice as likely to come up.
                indices.sort_unstable();
                indices.dedup();
                indices
            }
        }
    }
}

fn is_preset(path: &Path) -> bool {
    !is_junk(path)
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("milk"))
}

/// Whether a file is something the OS dropped into the pack rather than part
/// of it: Finder's .DS_Store, and the AppleDouble `._name` sidecars macOS
/// writes beside every file on a volume that can't hold resource forks (SMB,
/// exFAT, a USB stick). A sidecar keeps the real name's extension, so
/// unzipping a pack off a stick leaves a `._preset.milk` beside every preset
/// and the rotation fills up with files projectM can only fail to parse.
///
/// Same rule `rox_library::scanner::is_junk` runs on audio files, written out
/// again here because this crate reaches no higher than rox-viz (ADR 28).
fn is_junk(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    name == ".DS_Store" || name.starts_with("._")
}

fn is_textures_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("textures"))
}

/// The shuffle behind `Command::NextPreset`.
///
/// This is xorshift64* seeded off the clock rather than `rand`, which the
/// tree only carries transitively and in three different major versions.
/// Picking a preset out of a list is not where randomness quality matters,
/// and a dozen lines here beats pinning a fourth copy of a dependency.
pub struct Shuffle {
    state: u64,
}

impl Shuffle {
    pub fn new() -> Shuffle {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545_F491_4F6C_DD1D);
        Shuffle::from_seed(nanos)
    }

    pub fn from_seed(seed: u64) -> Shuffle {
        // Zero is xorshift's fixed point, so it never gets to be the state.
        Shuffle { state: seed | 1 }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// An index below `len`, avoiding `except` when there's anything else to
    /// pick. One retry, not a loop: two presets and bad luck shouldn't spin.
    pub fn pick(&mut self, len: usize, except: Option<usize>) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let mut index = (self.next_u64() % len as u64) as usize;
        if Some(index) == except && len > 1 {
            index = (index + 1) % len;
        }
        Some(index)
    }
}

impl Default for Shuffle {
    fn default() -> Self {
        Shuffle::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn scan_finds_nested_presets_and_ignores_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("b.milk"));
        touch(&root.join("nested/deeper/a.MILK"));
        touch(&root.join("nested/notes.txt"));
        touch(&root.join("milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert_eq!(
            library.presets(),
            &[root.join("b.milk"), root.join("nested/deeper/a.MILK")]
        );
        assert!(!library.is_empty());
    }

    /// What a pack unzipped off a USB stick or an SMB share on macOS looks
    /// like: a `._name` sidecar beside every file, plus a .DS_Store per
    /// folder. The sidecars carry the .milk extension, so they are the ones
    /// that would otherwise land in the rotation.
    #[test]
    fn scan_skips_the_junk_macos_leaves_beside_presets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("a.milk"));
        touch(&root.join("._a.milk"));
        touch(&root.join(".DS_Store"));
        touch(&root.join("nested/._b.MILK"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert_eq!(library.presets(), &[root.join("a.milk")]);

        // Favorites come in as explicit paths, so they take the same filter
        // rather than trusting whatever was starred.
        let mut library = library;
        library.extend(&[root.join("._a.milk")]);
        assert_eq!(library.presets(), &[root.join("a.milk")]);
    }

    #[test]
    fn scan_sorts_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["c.milk", "a.milk", "b.milk"] {
            touch(&root.join(name));
        }

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert_eq!(
            library.presets(),
            &[
                root.join("a.milk"),
                root.join("b.milk"),
                root.join("c.milk")
            ]
        );
    }

    #[test]
    fn scan_of_an_empty_or_missing_root_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let library = PresetLibrary::scan(
            &[dir.path().to_path_buf(), dir.path().join("does-not-exist")],
            None,
        );
        assert!(library.is_empty());
        assert_eq!(library.presets(), &[] as &[PathBuf]);
    }

    #[test]
    fn overlapping_roots_yield_each_preset_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("pack/one.milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf(), root.join("pack")], None);
        assert_eq!(library.presets(), &[root.join("pack/one.milk")]);
    }

    #[test]
    fn textures_and_roots_are_kept_for_projectm() {
        let dir = tempfile::tempdir().unwrap();
        let textures = dir.path().join("textures");
        let library = PresetLibrary::scan(&[dir.path().to_path_buf()], Some(textures.clone()));
        assert_eq!(library.textures(), &[textures]);
        assert_eq!(library.roots(), &[dir.path().to_path_buf()]);
    }

    #[test]
    fn scan_picks_up_texture_folders_shipped_inside_packs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let own = root.join("own-textures");
        touch(&root.join("pack/presets/a.milk"));
        touch(&root.join("pack/textures/001.jpg"));
        touch(&root.join("other/Textures/b.png"));
        touch(&root.join("other/textures.txt"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], Some(own.clone()));
        assert_eq!(
            library.textures(),
            &[own, root.join("other/Textures"), root.join("pack/textures")]
        );
        assert_eq!(library.presets(), &[root.join("pack/presets/a.milk")]);
    }

    #[test]
    fn a_texture_folder_named_explicitly_and_found_by_the_walk_is_listed_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let textures = root.join("textures");
        touch(&textures.join("001.jpg"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], Some(textures.clone()));
        assert_eq!(library.textures(), &[textures]);
    }

    #[test]
    fn index_of_locates_a_preset_in_the_sorted_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["a.milk", "b.milk"] {
            touch(&root.join(name));
        }

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert_eq!(library.index_of(&root.join("b.milk")), Some(1));
        assert_eq!(library.index_of(&root.join("gone.milk")), None);
    }

    #[test]
    fn folders_lists_only_directories_that_hold_presets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Fractal/one.milk"));
        touch(&root.join("Fractal/deep/two.milk"));
        touch(&root.join("Dancer/three.milk"));
        // A category with nothing but a subfolder and some support files.
        touch(&root.join("Geometric/textures/tile.png"));
        touch(&root.join("Geometric/inner/four.milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert_eq!(
            library.folders(),
            vec![
                root.join("Dancer"),
                root.join("Fractal"),
                root.join("Fractal/deep"),
                root.join("Geometric/inner"),
            ]
        );
    }

    #[test]
    fn folders_lists_a_directory_once_however_many_presets_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["a.milk", "b.milk", "c.milk"] {
            touch(&root.join("Fractal").join(name));
        }

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert_eq!(library.folders(), vec![root.join("Fractal")]);
    }

    #[test]
    fn rotation_all_selects_the_whole_library() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Fractal/one.milk"));
        touch(&root.join("Dancer/two.milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert_eq!(library.rotation_indices(&Rotation::All), vec![0, 1]);
        assert_eq!(library.rotation_indices(&Rotation::default()), vec![0, 1]);
    }

    #[test]
    fn a_folder_rotation_takes_everything_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Dancer/a.milk"));
        touch(&root.join("Fractal/b.milk"));
        touch(&root.join("Fractal/deep/c.milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        let indices = library.rotation_indices(&Rotation::Folder(root.join("Fractal")));
        let picked: Vec<&PathBuf> = indices.iter().map(|i| &library.presets()[*i]).collect();
        assert_eq!(
            picked,
            vec![
                &root.join("Fractal/b.milk"),
                &root.join("Fractal/deep/c.milk")
            ]
        );
    }

    #[test]
    fn a_folder_rotation_that_matches_nothing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Fractal/a.milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        assert!(
            library
                .rotation_indices(&Rotation::Folder(root.join("Deleted")))
                .is_empty()
        );
    }

    #[test]
    fn a_folder_rotation_does_not_swallow_a_sibling_with_the_same_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Fractal/a.milk"));
        touch(&root.join("Fractal Extras/b.milk"));
        touch(&root.join("Fractalish/c.milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        let indices = library.rotation_indices(&Rotation::Folder(root.join("Fractal")));
        let picked: Vec<&PathBuf> = indices.iter().map(|i| &library.presets()[*i]).collect();
        assert_eq!(picked, vec![&root.join("Fractal/a.milk")]);
    }

    #[test]
    fn a_set_rotation_keeps_only_what_the_library_holds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Dancer/a.milk"));
        touch(&root.join("Fractal/b.milk"));
        touch(&root.join("Fractal/c.milk"));

        let library = PresetLibrary::scan(&[root.to_path_buf()], None);
        let indices = library.rotation_indices(&Rotation::Set(vec![
            root.join("Fractal/c.milk"),
            root.join("Dancer/a.milk"),
            // Twice, and gone: neither should show.
            root.join("Fractal/c.milk"),
            root.join("Fractal/deleted.milk"),
        ]));
        assert_eq!(indices, vec![0, 2]);
        assert!(
            library
                .rotation_indices(&Rotation::Set(Vec::new()))
                .is_empty()
        );
    }

    #[test]
    fn extend_folds_in_files_from_outside_the_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("pack/a.milk"));
        touch(&root.join("elsewhere/z.milk"));
        touch(&root.join("elsewhere/notes.txt"));

        let mut library = PresetLibrary::scan(&[root.join("pack")], None);
        library.extend(&[
            root.join("elsewhere/z.milk"),
            // Already there, not a preset, and not on disk: none of these
            // change the list.
            root.join("pack/a.milk"),
            root.join("elsewhere/notes.txt"),
            root.join("elsewhere/gone.milk"),
        ]);
        assert_eq!(
            library.presets(),
            &[root.join("elsewhere/z.milk"), root.join("pack/a.milk")]
        );
        // Still findable by the sorted lookup after the merge.
        assert_eq!(library.index_of(&root.join("pack/a.milk")), Some(1));
    }

    #[test]
    fn shuffle_stays_in_range_and_avoids_the_current_pick() {
        let mut shuffle = Shuffle::from_seed(12345);
        for _ in 0..200 {
            let index = shuffle.pick(8, Some(3)).unwrap();
            assert!(index < 8);
            assert_ne!(index, 3);
        }
        // A one-preset library has nowhere else to go, so "except" loses.
        assert_eq!(shuffle.pick(1, Some(0)), Some(0));
        assert_eq!(shuffle.pick(0, None), None);
    }

    #[test]
    fn shuffle_does_not_get_stuck_on_one_value() {
        let mut shuffle = Shuffle::from_seed(1);
        let first = shuffle.pick(64, None).unwrap();
        assert!((0..200).any(|_| shuffle.pick(64, None).unwrap() != first));
    }
}

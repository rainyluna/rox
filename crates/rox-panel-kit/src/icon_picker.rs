//! The icon picker: the field a custom button's editor drops its glyph
//! list from. The machinery is
//! [`search_picker`](crate::search_picker::search_picker), the same one
//! the font and language fields use; this file turns
//! [`icons::CATALOG`](rox_design::assets::icons::CATALOG) into rows and
//! nothing more.
//!
//! It's a search field rather than a dropdown because the catalog runs
//! past a hundred entries, and a plain menu at that length is a scroll
//! hunt. Every row carries its own glyph, since a list of icon names
//! with no icons in it asks the reader to remember what "rows 2" looks
//! like.
//!
//! The built-in catalog is the whole offerable set. Nothing here reads
//! icon packs or anything else the user supplies: there's no path in the
//! tree that renders arbitrary user bytes as a panel glyph, and giving
//! one to a picker would be inventing it here.

use std::sync::{Arc, OnceLock};

use gpui::{Context, SharedString, prelude::*};

use rox_design::assets::icons;

use crate::search_picker::{PickRow, search_picker};

/// What the closed field reads when nothing is set, or when the stored
/// path isn't a catalog entry any more. Untranslated like the rest of
/// this picker's copy, since the names it sits among are file stems.
const UNSET_LABEL: &str = "None";

/// A searchable dropdown over every icon the app draws. `current` is the
/// stored catalog path, empty for unset; a path the catalog no longer
/// has reads as unset, the same thing a draw of it would come to. The
/// apply hands back the picked path.
// `use<..>` and the named `A` for the same reason as the crate root's
// `picker`.
pub fn icon_picker<P, A>(
    id: &'static str,
    current: &str,
    apply: A,
    cx: &mut Context<P>,
) -> impl IntoElement + use<P, A>
where
    P: 'static,
    A: Fn(&mut P, SharedString, &mut Context<P>) + Clone + 'static,
{
    let rows = rows();

    let current: Option<SharedString> = icons::CATALOG
        .iter()
        .find(|path| **path == current)
        .map(|path| SharedString::from(*path));

    let label = rows
        .iter()
        .find(|row| row.value == current)
        .map(|row| row.label.clone())
        .unwrap_or_else(|| UNSET_LABEL.into());

    search_picker(
        id,
        rows,
        label,
        current,
        "Search icons".into(),
        "No matches".into(),
        move |this, value, cx| {
            // Every row here carries a path, so the clear-to-default head
            // the shared field allows never turns up on this list.
            if let Some(value) = value {
                apply(this, value.into(), cx);
            }
        },
        cx,
    )
}

/// Every icon the app draws, built once and cached. Rebuilt never: the
/// catalog is static for the life of the process, and this used to be a
/// settings render's problem when the same list was rebuilt per frame.
fn rows() -> Arc<Vec<PickRow>> {
    static ROWS: OnceLock<Arc<Vec<PickRow>>> = OnceLock::new();

    ROWS.get_or_init(|| Arc::new(icons::CATALOG.iter().copied().map(row).collect()))
        .clone()
}

/// One catalog path as a row. The catalog holds paths and nothing in the
/// tree holds display names for them, so the name is derived: the file
/// stem with its hyphens opened out, which turns `icons/skip-back.svg`
/// into "skip back".
fn row(path: &'static str) -> PickRow {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.strip_suffix(".svg").unwrap_or(file);

    // The whole stem plus each of its words, so "skip" and "back" both
    // find skip-back. Folded on the way in because the filter matches
    // terms raw.
    let mut terms: Vec<SharedString> = vec![stem.to_lowercase().into()];
    terms.extend(
        stem.split('-')
            .filter(|word| *word != stem)
            .map(|word| SharedString::from(word.to_lowercase())),
    );

    PickRow {
        label: stem.replace('-', " ").into(),
        value: Some(path.into()),
        terms,
        icon: Some(path.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row for a known catalog path, by the value it stores.
    fn row_for(rows: &[PickRow], path: &'static str) -> PickRow {
        rows.iter()
            .find(|row| row.value == Some(SharedString::from(path)))
            .unwrap_or_else(|| panic!("{path} is in the catalog"))
            .clone()
    }

    #[test]
    fn every_catalog_icon_becomes_a_row() {
        assert_eq!(rows().len(), icons::CATALOG.len());
    }

    #[test]
    fn labels_are_readable() {
        let rows = rows();

        assert_eq!(row_for(&rows, icons::SKIP_BACK).label, "skip back");

        for row in rows.iter() {
            let label: &str = row.label.as_ref();
            assert!(!label.contains(".svg"), "{label} kept its extension");
            assert!(!label.contains('/'), "{label} kept its folder");
        }
    }

    #[test]
    fn terms_cover_each_word() {
        let back = row_for(&rows(), icons::SKIP_BACK);

        let terms: Vec<&str> = back.terms.iter().map(|term| term.as_ref()).collect();
        assert!(terms.contains(&"skip"), "{terms:?} lost the first word");
        assert!(terms.contains(&"back"), "{terms:?} lost the second word");
    }

    #[test]
    fn values_round_trip_to_catalog_paths() {
        // The one the button editor rides on: a stored value that isn't a
        // catalog path draws as the unconfigured placeholder, and the user
        // gets a button that quietly lost its icon.
        for row in rows().iter() {
            let value: &str = row
                .value
                .as_ref()
                .expect("every icon row sets a value")
                .as_ref();

            assert!(
                icons::CATALOG.contains(&value),
                "{value} is not a catalog path"
            );
        }
    }
}

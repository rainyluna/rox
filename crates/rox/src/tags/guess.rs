//! Filename pattern guessing: tag values pulled out of a path by a
//! format string, foobar2000's masstagger idea. A pattern mixes literal
//! text with %field% placeholders ("%artist% - %title%") and matches
//! against the file stem; a "/" in the pattern steps up the path, so
//! "%artist% - %album%/%track%. %title%" reads the folder name too.
//! %skip% swallows a segment without keeping it. Matching is non-greedy:
//! a capture takes the shortest text that lets the rest of the pattern
//! match, so the first " - " splits artist from title even when the title
//! contains one. Captures trim their edges and must be non-empty. The
//! editor previews every track through [`Pattern::apply`] before
//! anything is written, so a bad pattern costs nothing.
//!
//! Reading a path is all this module does. The same pattern runs the
//! other way through [`rox_core::pattern`], values in and a relative path
//! out, which file renaming and conversion output naming are both built
//! on, and which the stream capture down in the services shares. The
//! parse is one parse; only the direction differs.

use std::path::{Path, PathBuf};

use rox_core::pattern::{PatternField, Token};
use rox_library::writer::Field;

pub use rox_core::pattern::PLACEHOLDERS;

/// The tag fields as a pattern vocabulary: which placeholder names this
/// crate answers to, and what each one renders as. The engine down in
/// [`rox_core::pattern`] never learns what a tag is, so this is where
/// that gets said.
///
/// A wrapper rather than the bare field because the trait and the field
/// are each defined in a crate that isn't this one, and Rust won't take
/// an impl from a third. It stays inside this module: everything outside
/// hands over and takes back plain [`Field`] values.
#[derive(Clone, PartialEq)]
struct TagField(Field);

impl PatternField for TagField {
    /// Unknown names are a parse error, not a literal: a typoed
    /// `%tittle%` silently matching as text would be far harder to spot
    /// in the preview.
    fn from_placeholder(name: &str) -> Result<Option<Self>, String> {
        Ok(Some(TagField(match name {
            "artist" => Field::Artist,
            "albumartist" | "album artist" => Field::AlbumArtist,
            "album" => Field::Album,
            "title" => Field::Title,
            "track" | "tracknumber" => Field::TrackNo,
            "disc" | "discnumber" => Field::DiscNo,
            "year" | "date" => Field::Year,
            "genre" => Field::Genre,
            "comment" => Field::Comment,
            "skip" | "dummy" | "ignore" => return Ok(None),
            other => {
                return Err(rox_i18n::t!(
                    "tags-guess-unknown-placeholder",
                    name = other.to_owned()
                )
                .to_string());
            }
        })))
    }

    /// What a field renders as when the track has nothing for it. No tag
    /// field is allowed to vanish: a missing album would collapse a
    /// folder level and drop the file somewhere it doesn't belong. These
    /// double as the sanitizer's fallback, so a value of pure punctuation
    /// ends up here too.
    fn fallback(&self) -> &'static str {
        match self.0 {
            Field::Artist | Field::AlbumArtist => "Unknown Artist",
            Field::Album => "Unknown Album",
            Field::Title => "Untitled",
            Field::TrackNo | Field::DiscNo => "00",
            Field::Year => "Unknown Year",
            Field::Genre => "Unknown Genre",
            // %comment% lands here: a track with no comment renders like
            // the rest of them. No other field has a placeholder, so
            // nothing else reaches this from a parsed pattern. Kept total
            // rather than panicking.
            _ => "Unknown",
        }
    }

    fn value(&self, raw: &str) -> String {
        match self.0 {
            Field::TrackNo | Field::DiscNo => padded(raw),
            _ => raw.trim().to_owned(),
        }
    }
}

/// A track or disc number as two digits, so 3 sorts before 12 in every
/// file browser. ID3's "3/12" total form keeps only the number. Anything
/// that isn't a plain number (a "A1" vinyl side) is left alone.
fn padded(value: &str) -> String {
    let head = value.split('/').next().unwrap_or(value).trim();

    match head.parse::<u32>() {
        Ok(n) => format!("{n:02}"),
        Err(_) => value.trim().to_owned(),
    }
}

/// A parsed pattern over the tag fields, matching and rendering both.
pub struct Pattern(rox_core::pattern::Pattern<TagField>);

/// Parse `text` into a pattern, or say what is wrong with it: an unknown
/// placeholder, an unclosed %, or nothing to capture at all.
pub fn parse(text: &str) -> Result<Pattern, String> {
    rox_core::pattern::parse(text).map(Pattern)
}

/// Match `tokens` against `text` from the front, non-greedy, collecting
/// captures into `out`. On failure `out` is left as it was.
fn match_tokens(tokens: &[Token<TagField>], text: &str, out: &mut Vec<(Field, String)>) -> bool {
    let Some(token) = tokens.first() else {
        return text.is_empty();
    };
    match token {
        Token::Literal(lit) => match text.strip_prefix(lit.as_str()) {
            Some(rest) => match_tokens(&tokens[1..], rest, out),
            None => false,
        },
        Token::Capture(_) | Token::Skip => {
            // Shortest capture first: every char boundary is a candidate
            // split, and the first one the rest of the pattern accepts
            // wins. A trailing capture takes the whole remainder in one
            // step since only the empty tail can close the list.
            let ends = text
                .char_indices()
                .map(|(i, _)| i)
                .skip(1)
                .chain([text.len()]);
            for end in ends {
                let (taken, rest) = text.split_at(end);
                let trimmed = taken.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let mark = out.len();
                if let Token::Capture(field) = token {
                    out.push((field.0.clone(), trimmed.to_owned()));
                }
                if match_tokens(&tokens[1..], rest, out) {
                    return true;
                }
                out.truncate(mark);
            }
            false
        }
    }
}

impl Pattern {
    /// Whether the pattern has a folder part at all: `%track% - %title%`
    /// is a file name alone, `%album%/%title%` names a folder above it.
    pub fn has_folders(&self) -> bool {
        self.0.has_folders()
    }

    /// Run the pattern forwards: tag values in, a relative path out. The
    /// rendering itself is [`rox_core::pattern::Pattern::render`]; the
    /// pass here only puts the caller's fields into the wrapper the
    /// engine is parameterized over.
    pub fn render(&self, values: &[(Field, String)]) -> Result<PathBuf, String> {
        let values: Vec<(TagField, String)> = values
            .iter()
            .map(|(field, value)| (TagField(field.clone()), value.clone()))
            .collect();

        self.0.render(&values)
    }

    /// Run the pattern over `path`: the last component against the file
    /// stem, earlier ones against the folders above it. A field captured
    /// twice keeps the deepest hit, so the filename outranks the folder
    /// when both name the artist. None when any component fails to
    /// match, including a pattern deeper than the path itself.
    pub fn apply(&self, path: &Path) -> Option<Vec<(Field, String)>> {
        let components = self.0.components();
        let mut names: Vec<String> = Vec::with_capacity(components.len());
        let mut at = path;
        for i in 0..components.len() {
            let name = if i == 0 {
                at.file_stem()?.to_str()?.to_owned()
            } else {
                at.file_name()?.to_str()?.to_owned()
            };
            names.push(name);
            at = at.parent()?;
        }
        names.reverse();
        let mut captures = Vec::new();
        for (tokens, name) in components.iter().zip(&names) {
            if !match_tokens(tokens, name, &mut captures) {
                return None;
            }
        }
        // Deepest capture of a field wins; matching walked shallow to
        // deep, so keep each field's last entry.
        let mut deduped: Vec<(Field, String)> = Vec::with_capacity(captures.len());
        for (field, value) in captures.into_iter().rev() {
            if !deduped.iter().any(|(f, _)| *f == field) {
                deduped.push((field, value));
            }
        }
        deduped.reverse();
        Some(deduped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn apply(pattern: &str, path: &str) -> Option<Vec<(Field, String)>> {
        parse(pattern).unwrap().apply(&PathBuf::from(path))
    }

    #[test]
    fn artist_and_title_split_on_first_separator() {
        let got = apply("%artist% - %title%", "/m/Boards - Roygbiv - Live.mp3").unwrap();
        assert_eq!(
            got,
            vec![
                (Field::Artist, "Boards".into()),
                (Field::Title, "Roygbiv - Live".into()),
            ]
        );
    }

    #[test]
    fn folder_component_reads_album() {
        let got = apply(
            "%artist% - %album%/%artist% - %title%",
            "/m/Boards - Geogaddi/Boards - Julie.flac",
        )
        .unwrap();
        assert_eq!(
            got,
            vec![
                (Field::Album, "Geogaddi".into()),
                (Field::Artist, "Boards".into()),
                (Field::Title, "Julie".into()),
            ]
        );
    }

    #[test]
    fn leading_track_number_variants() {
        let got = apply("%track%. %title%", "/m/01. Intro.mp3").unwrap();
        assert_eq!(
            got,
            vec![
                (Field::TrackNo, "01".into()),
                (Field::Title, "Intro".into())
            ]
        );
        let got = apply("%track% - %title%", "/m/07 - Outro.mp3").unwrap();
        assert_eq!(
            got,
            vec![
                (Field::TrackNo, "07".into()),
                (Field::Title, "Outro".into())
            ]
        );
    }

    #[test]
    fn skip_swallows_without_capturing() {
        let got = apply("%skip% - %title%", "/m/03 - Song.mp3").unwrap();
        assert_eq!(got, vec![(Field::Title, "Song".into())]);
    }

    #[test]
    fn deepest_capture_wins_on_duplicate_fields() {
        let got = apply(
            "%artist% - %album%/%artist% - %title%",
            "/m/Various - Comp/Actual Artist - Song.mp3",
        )
        .unwrap();
        assert!(got.contains(&(Field::Artist, "Actual Artist".into())));
        assert!(!got.iter().any(|(_, v)| v == "Various"));
    }

    #[test]
    fn no_match_is_none() {
        assert!(apply("%track%. %title%", "/m/No Number Here.mp3").is_none());
    }

    #[test]
    fn captures_trim_but_never_go_empty() {
        assert!(apply("%artist% - %title%", "/m/ - Title.mp3").is_none());
        let got = apply("%artist%-%title%", "/m/Name - Song.mp3").unwrap();
        assert_eq!(
            got,
            vec![
                (Field::Artist, "Name".into()),
                (Field::Title, "Song".into())
            ]
        );
    }

    fn render(pattern: &str, values: &[(Field, &str)]) -> Result<String, String> {
        let values: Vec<(Field, String)> = values
            .iter()
            .map(|(f, v)| (f.clone(), (*v).to_owned()))
            .collect();

        parse(pattern)
            .unwrap()
            .render(&values)
            .map(|p| p.to_string_lossy().into_owned())
    }

    #[test]
    fn numbers_pad_to_two_digits() {
        assert_eq!(
            render(
                "%disc%-%track% %title%",
                &[
                    (Field::DiscNo, "2"),
                    (Field::TrackNo, "7/12"),
                    (Field::Title, "Outro"),
                ]
            )
            .unwrap(),
            "02-07 Outro"
        );
        // Past two digits the number keeps its own width, and a side
        // marker that isn't a number is left as it was typed.
        assert_eq!(
            render(
                "%track% %title%",
                &[(Field::TrackNo, "123"), (Field::Title, "X")]
            )
            .unwrap(),
            "123 X"
        );
        assert_eq!(
            render(
                "%track% %title%",
                &[(Field::TrackNo, "A1"), (Field::Title, "X")]
            )
            .unwrap(),
            "A1 X"
        );
    }

    #[test]
    fn no_tag_field_vanishes_from_a_name() {
        assert_eq!(
            render(
                "%albumartist%/%album%/%track% %title%",
                &[(Field::Title, "Song")]
            )
            .unwrap(),
            "Unknown Artist/Unknown Album/00 Song"
        );
    }

    #[test]
    fn render_round_trips_through_apply() {
        let pattern = "%albumartist%/%album%/%track% - %title%";
        let values = [
            (Field::AlbumArtist, "Boards of Canada".to_owned()),
            (Field::Album, "Geogaddi".to_owned()),
            (Field::TrackNo, "4".to_owned()),
            (Field::Title, "Julie and Candy".to_owned()),
        ];
        let rendered = parse(pattern).unwrap().render(&values).unwrap();
        assert_eq!(
            rendered,
            PathBuf::from("Boards of Canada/Geogaddi/04 - Julie and Candy")
        );
        let full = PathBuf::from("/music")
            .join(&rendered)
            .with_extension("flac");
        let mut back = parse(pattern).unwrap().apply(&full).unwrap();
        back.sort_by_key(|(f, _)| format!("{f:?}"));
        let mut want: Vec<(Field, String)> = values.to_vec();
        want[2].1 = "04".to_owned();
        want.sort_by_key(|(f, _)| format!("{f:?}"));
        assert_eq!(back, want);
    }
}

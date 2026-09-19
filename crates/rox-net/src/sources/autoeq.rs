//! AutoEq database client and profile parser.
//!
//! AutoEq (https://github.com/jaakkopasanen/AutoEq) provides thousands of
//! headphone and in-ear monitor frequency response corrections measured by
//! oratory1990, Crinacle, Rtings, Super Review, and squig.link reviewers.
//!
//! Each profile in the repository includes a precomputed `FixedBandEQ.txt`
//! specifically optimized for 10-band graphic equalizers on the standard
//! ISO octave bands (31/32, 62/64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000 Hz).
//!
//! This module parses the master index (`INDEX.md`), parses EQ formats
//! (FixedBandEQ, ParametricEQ, GraphicEQ, and CSV), and provides live
//! search and fetching routines.

use std::cmp::Ordering;

use crate::providers::{agent, net_reason};

/// The 10 standard ISO octave center frequencies in Hz used by graphic equalizers.
pub const BAND_HZ: [f32; 10] = [
    32.0, 64.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// How many bands there are in the graphic equalizer.
pub const BANDS: usize = BAND_HZ.len();

/// The maximum cut or boost in dB supported by the equalizer.
pub const GAIN_MAX_DB: f32 = 12.0;

/// The raw GitHub URL for the AutoEq master results index.
pub const INDEX_URL: &str =
    "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/INDEX.md";

/// An entry in the AutoEq index representing a headphone model measurement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoEqEntry {
    /// The headphone or IEM model name, e.g. "Sennheiser HD 600".
    pub name: String,
    /// The relative path in the results repository, e.g. "oratory1990/over-ear/Sennheiser HD 600".
    pub path: String,
    /// The measurement source / rig, e.g. "oratory1990" or "crinacle on 711".
    pub source: String,
}

/// A parsed equalizer profile with 10 band gains and optional preamp.
#[derive(Clone, Debug, PartialEq)]
pub struct AutoEqProfile {
    pub name: String,
    pub preamp_db: Option<f32>,
    pub gains_db: [f32; BANDS],
}

/// Parse the AutoEq `INDEX.md` content into a list of [`AutoEqEntry`].
pub fn parse_index(text: &str) -> Vec<AutoEqEntry> {
    let mut entries = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        // Standard AutoEq INDEX.md line format:
        // - [Model Name](./path/to/dir) by reviewer on rig
        let Some(rest) = line.strip_prefix("- [") else {
            continue;
        };
        let Some(close_bracket) = rest.find("](") else {
            continue;
        };
        let name = &rest[..close_bracket];
        let after_bracket = &rest[close_bracket + 2..];
        let Some(close_paren) = after_bracket.find(')') else {
            continue;
        };
        let raw_path = &after_bracket[..close_paren];
        let path = raw_path.strip_prefix("./").unwrap_or(raw_path);
        let after_paren = after_bracket[close_paren + 1..].trim();
        let source = after_paren.strip_prefix("by ").unwrap_or(after_paren);

        if !name.is_empty() && !path.is_empty() {
            entries.push(AutoEqEntry {
                name: name.to_string(),
                path: path.to_string(),
                source: source.to_string(),
            });
        }
    }

    entries
}

/// Filter and rank entries matching a multi-word search query.
pub fn filter_entries<'a>(
    entries: &'a [AutoEqEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a AutoEqEntry> {
    let query = query.trim();
    if query.is_empty() {
        return entries.iter().take(limit).collect();
    }

    let terms: Vec<String> = query.split_whitespace().map(|s| s.to_lowercase()).collect();

    let mut matches: Vec<(&'a AutoEqEntry, usize)> = entries
        .iter()
        .filter_map(|entry| {
            let name_lower = entry.name.to_lowercase();
            let source_lower = entry.source.to_lowercase();

            // All terms must appear in either name or source.
            let all_match = terms
                .iter()
                .all(|t| name_lower.contains(t) || source_lower.contains(t));

            if !all_match {
                return None;
            }

            // Score: lower is better.
            let full_lower = query.to_lowercase();
            let score = if name_lower == full_lower {
                0
            } else if name_lower.starts_with(&full_lower) {
                1
            } else if name_lower.contains(&full_lower) {
                2
            } else {
                3 + entry.name.len()
            };

            Some((entry, score))
        })
        .collect();

    matches.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.name.cmp(&b.0.name)));
    matches.into_iter().take(limit).map(|(e, _)| e).collect()
}

/// The raw GitHub URL for a profile's `FixedBandEQ.txt`.
pub fn fixed_band_url(path: &str) -> String {
    let clean = path.trim_start_matches("./").trim_matches('/');
    let folder = clean.rsplit('/').next().unwrap_or(clean);
    format!(
        "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/{clean}/{folder}%20FixedBandEQ.txt"
    )
}

/// The raw GitHub URL for a profile's `ParametricEQ.txt` as fallback.
pub fn parametric_url(path: &str) -> String {
    let clean = path.trim_start_matches("./").trim_matches('/');
    let folder = clean.rsplit('/').next().unwrap_or(clean);
    format!(
        "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/{clean}/{folder}%20ParametricEQ.txt"
    )
}

/// Fetch the index content from GitHub. Blocking; run on background executor.
pub fn fetch_index() -> Result<String, String> {
    agent()
        .get(INDEX_URL)
        .call()
        .map_err(|e| net_reason(&e))?
        .into_string()
        .map_err(|e| e.to_string())
}

/// Fetch and parse a profile from GitHub. Blocking; run on background executor.
pub fn fetch_profile(path: &str, name: &str) -> Result<AutoEqProfile, String> {
    let url = fixed_band_url(path);
    let response = agent().get(&url).call();

    let body = match response {
        Ok(res) => res.into_string().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(404, _)) => {
            // Fallback to ParametricEQ.txt if FixedBandEQ.txt is not found
            let fallback_url = parametric_url(path);
            agent()
                .get(&fallback_url)
                .call()
                .map_err(|e| net_reason(&e))?
                .into_string()
                .map_err(|e| e.to_string())?
        }
        Err(e) => return Err(net_reason(&e)),
    };

    parse_profile(name, &body)
}

/// Find which band index in [`BAND_HZ`] a frequency is closest to (log scale).
fn closest_band(hz: f32) -> usize {
    if hz <= 0.0 {
        return 0;
    }
    let log_hz = hz.log10();
    BAND_HZ
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (log_hz - a.log10()).abs();
            let db = (log_hz - b.log10()).abs();
            da.partial_cmp(&db).unwrap_or(Ordering::Equal)
        })
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

/// Parse an equalizer profile from text.
///
/// Supports:
/// - AutoEq / Equalizer APO `FixedBandEQ.txt` & `ParametricEQ.txt`
/// - AutoEq / Wavelet / squig.link `GraphicEQ: ...` format
/// - Comma/space-separated CSV frequency response points
pub fn parse_profile(name: &str, text: &str) -> Result<AutoEqProfile, String> {
    let mut gains_db = [0.0f32; BANDS];
    let mut preamp_db = None;
    let mut has_filters = false;
    let mut graphic_points: Vec<(f32, f32)> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // 1. Preamp line: e.g. "Preamp: -7.5 dB"
        if line.to_lowercase().starts_with("preamp:") {
            if let Some(val_str) = line.split(':').nth(1) {
                let clean = val_str.to_lowercase().replace("db", "").trim().to_string();
                if let Ok(p) = clean.parse::<f32>() {
                    preamp_db = Some(p);
                }
            }
            continue;
        }

        // 2. GraphicEQ format: e.g. "GraphicEQ: 20 -0.3; 25 -0.4; 32 -1.1; ..."
        if line.to_lowercase().starts_with("graphiceq:") {
            let data = line.split(':').nth(1).unwrap_or("");
            for pair in data.split(';') {
                let parts: Vec<&str> = pair.split_whitespace().collect();
                if parts.len() >= 2
                    && let (Ok(hz), Ok(db)) = (parts[0].parse::<f32>(), parts[1].parse::<f32>())
                {
                    graphic_points.push((hz, db));
                }
            }
            continue;
        }

        // 3. Filter line: e.g. "Filter 1: ON PK Fc 31 Hz Gain 6.9 dB Q 1.41"
        if line.to_lowercase().starts_with("filter") {
            let lower = line.to_lowercase();
            // Only process enabled filters
            if !lower.contains(" on ") && !lower.contains(": on ") {
                continue;
            }

            let mut fc: Option<f32> = None;
            let mut gain: Option<f32> = None;

            let tokens: Vec<&str> = line.split_whitespace().collect();
            for i in 0..tokens.len() {
                if tokens[i].eq_ignore_ascii_case("fc") && i + 1 < tokens.len() {
                    fc = tokens[i + 1]
                        .replace("hz", "")
                        .replace("Hz", "")
                        .parse()
                        .ok();
                }
                if tokens[i].eq_ignore_ascii_case("gain") && i + 1 < tokens.len() {
                    gain = tokens[i + 1]
                        .replace("db", "")
                        .replace("dB", "")
                        .parse()
                        .ok();
                }
            }

            if let (Some(hz), Some(db)) = (fc, gain) {
                let band = closest_band(hz);
                gains_db[band] = db.clamp(-GAIN_MAX_DB, GAIN_MAX_DB);
                has_filters = true;
            }
            continue;
        }

        // 4. Fallback line: CSV / space-separated "freq gain"
        let parts: Vec<&str> = line
            .split(&[',', ' ', '\t'][..])
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() >= 2
            && let (Ok(hz), Ok(db)) = (parts[0].parse::<f32>(), parts[1].parse::<f32>())
        {
            graphic_points.push((hz, db));
        }
    }

    // If GraphicEQ or CSV points were found and no discrete filters parsed:
    if !has_filters && !graphic_points.is_empty() {
        graphic_points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        for (i, &band_hz) in BAND_HZ.iter().enumerate() {
            // Interpolate gain at band_hz
            let gain = interpolate_gain(&graphic_points, band_hz);
            gains_db[i] = gain.clamp(-GAIN_MAX_DB, GAIN_MAX_DB);
        }
        has_filters = true;
    }

    if !has_filters && preamp_db.is_none() {
        return Err("no valid filter settings found in profile".to_string());
    }

    Ok(AutoEqProfile {
        name: name.to_string(),
        preamp_db,
        gains_db,
    })
}

/// Interpolate a gain value at `target_hz` from a sorted list of `(freq, gain)` points.
fn interpolate_gain(points: &[(f32, f32)], target_hz: f32) -> f32 {
    if points.is_empty() {
        return 0.0;
    }
    if points.len() == 1 || target_hz <= points[0].0 {
        return points[0].1;
    }
    if target_hz >= points[points.len() - 1].0 {
        return points[points.len() - 1].1;
    }

    for window in points.windows(2) {
        let (f0, g0) = window[0];
        let (f1, g1) = window[1];
        if target_hz >= f0 && target_hz <= f1 {
            if (f1 - f0).abs() < 1e-6 {
                return g0;
            }
            // Linear interpolation in log10 frequency space
            let t = (target_hz.log10() - f0.log10()) / (f1.log10() - f0.log10());
            return g0 + t * (g1 - g0);
        }
    }

    points[points.len() - 1].1
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_INDEX: &str = r#"
# Index
This is a list of all equalization profiles.

- [1Custom SA02](./crinacle/711%20in-ear/1Custom%20SA02) by crinacle on 711
- [Sennheiser HD 600](./oratory1990/over-ear/Sennheiser%20HD%20600) by oratory1990
- [Sennheiser HD 600](./crinacle/GRAS%2043AG-7%20over-ear/Sennheiser%20HD%20600) by crinacle on GRAS 43AG-7
- [Apple AirPods Pro 2](./Rtings/in-ear/Apple%20AirPods%20Pro%202) by Rtings
"#;

    const SAMPLE_FIXED_BAND: &str = r#"
Preamp: -7.5 dB
Filter 1: ON PK Fc 31 Hz Gain 6.9 dB Q 1.41
Filter 2: ON PK Fc 62 Hz Gain 3.3 dB Q 1.41
Filter 3: ON PK Fc 125 Hz Gain -1.1 dB Q 1.41
Filter 4: ON PK Fc 250 Hz Gain -1.6 dB Q 1.41
Filter 5: ON PK Fc 500 Hz Gain 0.6 dB Q 1.41
Filter 6: ON PK Fc 1000 Hz Gain -0.8 dB Q 1.41
Filter 7: ON PK Fc 2000 Hz Gain 0.1 dB Q 1.41
Filter 8: ON PK Fc 4000 Hz Gain -1.0 dB Q 1.41
Filter 9: ON PK Fc 8000 Hz Gain 3.9 dB Q 1.41
Filter 10: ON PK Fc 16000 Hz Gain -6.5 dB Q 1.41
"#;

    const SAMPLE_GRAPHIC_EQ: &str = r#"
GraphicEQ: 20 -0.3; 32 6.9; 64 3.3; 125 -1.1; 250 -1.6; 500 0.6; 1000 -0.8; 2000 0.1; 4000 -1.0; 8000 3.9; 16000 -6.5; 20000 -8.0
"#;

    #[test]
    fn test_parse_index() {
        let entries = parse_index(SAMPLE_INDEX);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, "1Custom SA02");
        assert_eq!(entries[0].path, "crinacle/711%20in-ear/1Custom%20SA02");
        assert_eq!(entries[0].source, "crinacle on 711");

        assert_eq!(entries[1].name, "Sennheiser HD 600");
        assert_eq!(
            entries[1].path,
            "oratory1990/over-ear/Sennheiser%20HD%20600"
        );
        assert_eq!(entries[1].source, "oratory1990");
    }

    #[test]
    fn test_filter_entries() {
        let entries = parse_index(SAMPLE_INDEX);

        let hits = filter_entries(&entries, "hd 600", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "Sennheiser HD 600");

        let hits = filter_entries(&entries, "airpods", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Apple AirPods Pro 2");

        let hits = filter_entries(&entries, "oratory", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "oratory1990");
    }

    #[test]
    fn test_fixed_band_url() {
        let url = fixed_band_url("oratory1990/over-ear/Sennheiser%20HD%20600");
        assert_eq!(
            url,
            "https://raw.githubusercontent.com/jaakkopasanen/AutoEq/master/results/oratory1990/over-ear/Sennheiser%20HD%20600/Sennheiser%20HD%20600%20FixedBandEQ.txt"
        );
    }

    #[test]
    fn test_parse_fixed_band_profile() {
        let profile = parse_profile("Sennheiser HD 600", SAMPLE_FIXED_BAND).unwrap();
        assert_eq!(profile.name, "Sennheiser HD 600");
        assert_eq!(profile.preamp_db, Some(-7.5));
        assert_eq!(profile.gains_db[0], 6.9);
        assert_eq!(profile.gains_db[1], 3.3);
        assert_eq!(profile.gains_db[2], -1.1);
        assert_eq!(profile.gains_db[3], -1.6);
        assert_eq!(profile.gains_db[4], 0.6);
        assert_eq!(profile.gains_db[5], -0.8);
        assert_eq!(profile.gains_db[6], 0.1);
        assert_eq!(profile.gains_db[7], -1.0);
        assert_eq!(profile.gains_db[8], 3.9);
        assert_eq!(profile.gains_db[9], -6.5);
    }

    #[test]
    fn test_parse_graphic_eq_profile() {
        let profile = parse_profile("Test GraphicEQ", SAMPLE_GRAPHIC_EQ).unwrap();
        assert_eq!(profile.name, "Test GraphicEQ");
        assert!((profile.gains_db[0] - 6.9).abs() < 0.05);
        assert!((profile.gains_db[1] - 3.3).abs() < 0.05);
        assert!((profile.gains_db[2] - -1.1).abs() < 0.05);
        assert!((profile.gains_db[9] - -6.5).abs() < 0.05);
    }
}

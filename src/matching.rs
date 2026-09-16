//! Pick the profile that describes the connected outputs.
//!
//! A profile matches only if its outputs and the connected heads form a perfect
//! matching: every entry assigned, every head consumed. Entries that pin a serial
//! score higher than entries that only name a model, so a desk-specific profile
//! wins over the generic one for the same monitor models.

use anyhow::Result;

use crate::config::{Config, Output, Profile};
use crate::wl::{Head, HeadId, OutputSetting};

const SCORE_EXACT: u32 = 2;
const SCORE_MODEL: u32 = 1;

#[derive(Debug)]
pub struct Match<'a> {
    pub profile: &'a Profile,
    pub score: u32,
    /// Index into `heads` for each of the profile's outputs.
    pub assignment: Vec<usize>,
}

impl Match<'_> {
    pub fn settings(&self, heads: &[Head]) -> Result<Vec<OutputSetting>> {
        self.profile
            .outputs
            .iter()
            .zip(&self.assignment)
            .map(|(output, &head)| output.to_setting(&heads[head].id))
            .collect()
    }
}

/// The set of connected outputs, as the daemon compares it between hotplugs.
pub fn fingerprint(heads: &[Head]) -> Vec<HeadId> {
    let mut ids: Vec<HeadId> = heads.iter().map(|h| h.id.clone()).collect();
    ids.sort();
    ids
}

/// Whether one profile describes exactly these outputs, and how specifically.
pub fn match_profile<'a>(profile: &'a Profile, heads: &[Head]) -> Option<Match<'a>> {
    assign(profile, heads).map(|(score, assignment)| Match { profile, score, assignment })
}

/// Highest-scoring profile; ties go to the one earlier in the file.
pub fn best_match<'a>(config: &'a Config, heads: &[Head]) -> Option<Match<'a>> {
    let mut best: Option<Match<'a>> = None;
    for profile in &config.profiles {
        let Some(m) = match_profile(profile, heads) else { continue };
        if best.as_ref().is_none_or(|b| m.score > b.score) {
            best = Some(m);
        }
    }
    best
}

/// How well one entry describes one head. `None` means it does not apply at all.
fn score(output: &Output, head: &Head) -> Option<u32> {
    let mut exact = false;
    if let Some(connector) = &output.connector {
        if connector != &head.id.connector {
            return None;
        }
        exact = true;
    }
    if output.make.as_ref().is_some_and(|make| make != &head.id.make) {
        return None;
    }
    if output.model.as_ref().is_some_and(|model| model != &head.id.model) {
        return None;
    }
    if let Some(serial) = &output.serial {
        if serial != &head.id.serial {
            return None;
        }
        exact = true;
    }
    let names_nothing =
        output.connector.is_none() && output.make.is_none() && output.model.is_none() && output.serial.is_none();
    if names_nothing {
        return None;
    }
    Some(if exact { SCORE_EXACT } else { SCORE_MODEL })
}

fn assign(profile: &Profile, heads: &[Head]) -> Option<(u32, Vec<usize>)> {
    if heads.is_empty() || profile.outputs.len() != heads.len() {
        return None;
    }
    let scores: Vec<Vec<Option<u32>>> =
        profile.outputs.iter().map(|o| heads.iter().map(|h| score(o, h)).collect()).collect();

    let mut used = vec![false; heads.len()];
    let mut current = vec![0usize; profile.outputs.len()];
    let mut best = None;
    search(0, 0, &scores, &mut used, &mut current, &mut best);
    best
}

fn search(
    index: usize,
    total: u32,
    scores: &[Vec<Option<u32>>],
    used: &mut [bool],
    current: &mut [usize],
    best: &mut Option<(u32, Vec<usize>)>,
) {
    if index == current.len() {
        if best.as_ref().is_none_or(|(score, _)| total > *score) {
            *best = Some((total, current.to_vec()));
        }
        return;
    }
    for (head, cell) in scores[index].iter().enumerate() {
        let Some(score) = cell else { continue };
        if used[head] {
            continue;
        }
        used[head] = true;
        current[index] = head;
        search(index + 1, total + score, scores, used, current, best);
        used[head] = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wl::Mode;
    use wayland_client::protocol::wl_output::Transform;

    const PHILIPS: &str = "Philips Consumer Electronics Company";

    fn head(connector: &str, model: &str, serial: &str) -> Head {
        Head {
            id: HeadId {
                connector: connector.into(),
                make: PHILIPS.into(),
                model: model.into(),
                serial: serial.into(),
            },
            description: String::new(),
            modes: vec![Mode { width: 1920, height: 1080, refresh: 60_000, preferred: true }],
            enabled: true,
            current_mode: None,
            position: (0, 0),
            scale: 1.0,
            transform: Transform::Normal,
        }
    }

    fn internal() -> Head {
        let mut h = head("eDP-1", "Internal", "");
        h.id.make = "Unknown".into();
        h
    }

    fn external(model: &str, serial: Option<&str>) -> Output {
        Output {
            make: Some(PHILIPS.into()),
            model: Some(model.into()),
            serial: serial.map(str::to_string),
            position: Some([0, 0]),
            enabled: true,
            ..Output::default()
        }
    }

    fn edp() -> Output {
        Output { connector: Some("eDP-1".into()), enabled: true, ..Output::default() }
    }

    /// The real reason wano exists: one desk-specific profile plus one generic
    /// profile replace the hnz_other1..8 family.
    fn hot_desk_config() -> Config {
        Config {
            profiles: vec![
                Profile {
                    name: "hnz".into(),
                    outputs: vec![
                        external("PHL 241B7Q", Some("UHB2115009288")),
                        external("PHL 241B8Q", Some("ZV02017006149")),
                        edp(),
                    ],
                },
                Profile {
                    name: "hnz_any".into(),
                    outputs: vec![external("PHL 241B7Q", None), external("PHL 241B8Q", None), edp()],
                },
            ],
        }
    }

    #[test]
    fn exact_serials_win_over_the_generic_profile() {
        let heads =
            vec![head("DP-1", "PHL 241B7Q", "UHB2115009288"), head("DP-2", "PHL 241B8Q", "ZV02017006149"), internal()];
        let config = hot_desk_config();
        let m = best_match(&config, &heads).unwrap();
        assert_eq!(m.profile.name, "hnz");
        assert_eq!(m.score, SCORE_EXACT * 3);
    }

    #[test]
    fn another_desk_with_the_same_models_falls_back_to_the_generic_profile() {
        let heads =
            vec![head("DP-1", "PHL 241B7Q", "UHB2035003322"), head("DP-2", "PHL 241B8Q", "ZV02028008653"), internal()];
        let config = hot_desk_config();
        let m = best_match(&config, &heads).unwrap();
        assert_eq!(m.profile.name, "hnz_any");
    }

    #[test]
    fn entries_are_assigned_regardless_of_connector_order() {
        // Same monitors, swapped connectors: the assignment follows the models.
        let heads =
            vec![head("DP-5", "PHL 241B8Q", "ZV02017006149"), head("DP-3", "PHL 241B7Q", "UHB2115009288"), internal()];
        let config = hot_desk_config();
        let m = best_match(&config, &heads).unwrap();
        assert_eq!(m.profile.name, "hnz");
        assert_eq!(m.assignment, vec![1, 0, 2]);
        let settings = m.settings(&heads).unwrap();
        assert_eq!(settings[0].id.connector, "DP-3");
        assert_eq!(settings[1].id.connector, "DP-5");
    }

    #[test]
    fn an_unplugged_monitor_means_no_match() {
        let heads = vec![head("DP-1", "PHL 241B7Q", "UHB2115009288"), internal()];
        assert!(best_match(&hot_desk_config(), &heads).is_none());
    }

    #[test]
    fn an_extra_monitor_means_no_match() {
        let heads = vec![
            head("DP-1", "PHL 241B7Q", "UHB2115009288"),
            head("DP-2", "PHL 241B8Q", "ZV02017006149"),
            head("DP-4", "PHL 346B1C", "1322131231233"),
            internal(),
        ];
        assert!(best_match(&hot_desk_config(), &heads).is_none());
    }

    #[test]
    fn two_identical_models_are_both_assigned() {
        let config = Config {
            profiles: vec![Profile {
                name: "pair".into(),
                outputs: vec![external("PHL 241B7Q", Some("A")), external("PHL 241B7Q", Some("B"))],
            }],
        };
        let heads = vec![head("DP-1", "PHL 241B7Q", "B"), head("DP-2", "PHL 241B7Q", "A")];
        let m = best_match(&config, &heads).unwrap();
        assert_eq!(m.assignment, vec![1, 0]);
    }

    #[test]
    fn fingerprint_ignores_ordering() {
        let a = vec![head("DP-1", "M", "1"), head("DP-2", "M", "2")];
        let b = vec![head("DP-2", "M", "2"), head("DP-1", "M", "1")];
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn an_entry_that_names_nothing_never_matches() {
        let config = Config { profiles: vec![Profile { name: "empty".into(), outputs: vec![Output::default()] }] };
        assert!(best_match(&config, &[internal()]).is_none());
    }
}

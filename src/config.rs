//! Profile storage: `$XDG_CONFIG_HOME/wano/config.toml`, written by `wano save`
//! and by the arranger, read by everything.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::wl::{
    Head, HeadId, ModeSpec, OutputSetting, Snapshot, UNKNOWN, quantized_scale, transform_from_name, transform_name,
};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default, rename = "output")]
    pub outputs: Vec<Output>,
}

/// One output inside a profile. `connector`/`make`/`model`/`serial` are the match
/// keys; every other field is a setting, and `None` means "leave as-is".
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Output {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,

    #[serde(default = "enabled_default")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[i32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
}

fn enabled_default() -> bool {
    true
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from(".config"));
        base.join("wano/config.toml")
    }

    /// A missing config reads as empty, not as an error.
    pub fn read(path: &Path) -> Result<String> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).context("parsing config")
    }

    pub fn load(path: &Path) -> Result<Self> {
        Self::parse(&Self::read(path)?).with_context(|| format!("in {}", path.display()))
    }

    /// Write via a temporary file so a reader never sees a half-written config.
    pub fn save(&self, path: &Path) -> Result<()> {
        // Not to_string_pretty: that explodes `position` across four lines.
        let text = toml::to_string(self).context("serializing config")?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    /// Replace the profile of the same name, keeping its position in the file, or
    /// append it.
    pub fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
    }
}

impl Output {
    /// Snapshot one live output as a profile entry. Internal panels are matched by
    /// connector: their EDID serial is often absent or duplicated across machines.
    /// So are outputs that report no model at all, which would otherwise produce an
    /// entry that matches nothing.
    pub fn from_head(head: &Head, with_serial: bool) -> Self {
        let known = |s: &String| !s.is_empty() && s != UNKNOWN;
        let by_connector = is_internal(&head.id.connector) || !known(&head.id.model);
        Self {
            connector: by_connector.then(|| head.id.connector.clone()),
            make: (!by_connector).then(|| head.id.make.clone()).filter(known),
            model: (!by_connector).then(|| head.id.model.clone()).filter(known),
            serial: (!by_connector && with_serial).then(|| head.id.serial.clone()).filter(known),
            enabled: head.enabled,
            position: Some([head.position.0, head.position.1]),
            // The compositor reports scale quantized to wl_fixed (1.6 comes back
            // as 1.6015625). Snapping it to the 120ths wlroots uses keeps the file
            // readable and reapplies as the very same scale.
            scale: Some((quantized_scale(head.scale) * 1000.0).round() / 1000.0),
            mode: head.current_mode.map(|m| ModeSpec::from(m).to_string()),
            transform: Some(transform_name(head.transform).to_string()),
        }
    }

    pub fn to_setting(&self, id: &HeadId) -> Result<OutputSetting> {
        Ok(OutputSetting {
            id: id.clone(),
            enabled: self.enabled,
            position: self.position.map(|p| (p[0], p[1])),
            scale: self.scale,
            mode: self.mode.as_deref().map(str::parse::<ModeSpec>).transpose()?,
            transform: self.transform.as_deref().map(transform_from_name).transpose()?,
        })
    }

    /// How the entry reads in logs and in the UI.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.connector {
            parts.push(c.clone());
        }
        for field in [&self.make, &self.model, &self.serial].into_iter().flatten() {
            parts.push(field.clone());
        }
        if parts.is_empty() { "<matches nothing>".to_string() } else { parts.join(" ") }
    }
}

pub fn is_internal(connector: &str) -> bool {
    let c = connector.to_ascii_lowercase();
    c.starts_with("edp") || c.starts_with("lvds") || c.starts_with("dsi")
}

/// Build a profile from the live output state — the "remember this" primitive
/// shared by `wano save` and the arranger's Save button.
pub fn profile_from_snapshot(name: &str, snapshot: &Snapshot, with_serial: bool) -> Profile {
    Profile {
        name: name.to_string(),
        outputs: snapshot.heads.iter().map(|h| Output::from_head(h, with_serial)).collect(),
    }
}

/// A stable, readable name for the current set of outputs, e.g. `p2723qe-edp-1`.
pub fn suggested_name(snapshot: &Snapshot) -> String {
    let mut parts: Vec<String> = snapshot
        .heads
        .iter()
        .map(|h| {
            let raw = if is_internal(&h.id.connector) || h.id.model.is_empty() || h.id.model == UNKNOWN {
                &h.id.connector
            } else {
                &h.id.model
            };
            raw.to_ascii_lowercase()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("-")
        })
        .filter(|s| !s.is_empty())
        .collect();
    parts.sort();
    if parts.is_empty() { "profile".to_string() } else { parts.join("_") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wl::{Head, Mode};
    use wayland_client::protocol::wl_output::Transform;

    fn head(connector: &str, make: &str, model: &str, serial: &str) -> Head {
        Head {
            id: HeadId { connector: connector.into(), make: make.into(), model: model.into(), serial: serial.into() },
            description: String::new(),
            modes: vec![Mode { width: 1920, height: 1080, refresh: 60_000, preferred: true }],
            enabled: true,
            current_mode: Some(Mode { width: 1920, height: 1080, refresh: 60_000, preferred: true }),
            position: (0, 0),
            scale: 1.0,
            transform: Transform::Normal,
        }
    }

    #[test]
    fn internal_panels_match_by_connector() {
        let out = Output::from_head(&head("eDP-1", "Some Vendor", "Panel", "0x1234"), true);
        assert_eq!(out.connector.as_deref(), Some("eDP-1"));
        assert!(out.make.is_none() && out.serial.is_none());
    }

    #[test]
    fn external_monitors_match_by_model_and_optional_serial() {
        let h = head("DP-2", "Dell Inc.", "DELL P2723QE", "7HD1XY3");
        let with = Output::from_head(&h, true);
        assert_eq!(with.model.as_deref(), Some("DELL P2723QE"));
        assert_eq!(with.serial.as_deref(), Some("7HD1XY3"));
        assert!(with.connector.is_none());

        let without = Output::from_head(&h, false);
        assert!(without.serial.is_none());
        assert_eq!(without.model.as_deref(), Some("DELL P2723QE"));
    }

    #[test]
    fn unknown_edid_fields_are_dropped() {
        let out = Output::from_head(&head("DP-1", UNKNOWN, "PHL 241B7Q", UNKNOWN), true);
        assert!(out.make.is_none());
        assert!(out.serial.is_none());
        assert_eq!(out.model.as_deref(), Some("PHL 241B7Q"));
    }

    #[test]
    fn an_output_without_edid_falls_back_to_its_connector() {
        let out = Output::from_head(&head("WL-1", UNKNOWN, UNKNOWN, UNKNOWN), true);
        assert_eq!(out.connector.as_deref(), Some("WL-1"));
        assert!(out.make.is_none() && out.model.is_none() && out.serial.is_none());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let snapshot = Snapshot {
            serial: 1,
            heads: vec![head("DP-2", "Dell Inc.", "DELL P2723QE", "7HD1XY3"), head("eDP-1", "x", "y", "z")],
        };
        let mut config = Config::default();
        config.upsert(profile_from_snapshot("home_4k", &snapshot, true));

        let text = toml::to_string(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.profiles.len(), 1);
        let profile = back.profile("home_4k").unwrap();
        assert_eq!(profile.outputs.len(), 2);
        assert_eq!(profile.outputs[0].mode.as_deref(), Some("1920x1080@60.000"));
        assert_eq!(profile.outputs[1].connector.as_deref(), Some("eDP-1"));
    }

    #[test]
    fn upsert_replaces_in_place() {
        let mut config = Config {
            profiles: vec![
                Profile { name: "a".into(), outputs: vec![] },
                Profile { name: "b".into(), outputs: vec![] },
            ],
        };
        config.upsert(Profile { name: "a".into(), outputs: vec![Output::default()] });
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles[0].name, "a");
        assert_eq!(config.profiles[0].outputs.len(), 1);
    }

    #[test]
    fn suggested_name_is_stable_and_slugged() {
        let snapshot = Snapshot {
            serial: 1,
            heads: vec![head("eDP-1", "x", "y", "z"), head("DP-2", "Dell Inc.", "DELL P2723QE", "7HD1XY3")],
        };
        assert_eq!(suggested_name(&snapshot), "dell-p2723qe_edp-1");
    }
}

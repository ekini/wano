//! wlr-output-management client: take a snapshot of the outputs, apply a configuration.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context as _, Result, bail};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_output::Transform, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

/// kanshi's convention for an EDID field the display did not report.
pub const UNKNOWN: &str = "Unknown";

const MANAGER_VERSION: u32 = 4;

/// Identity of a connected output. Unique per head: two identical monitors still
/// differ by connector.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HeadId {
    pub connector: String,
    pub make: String,
    pub model: String,
    pub serial: String,
}

impl fmt::Display for HeadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} {} {})", self.connector, self.make, self.model, self.serial)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    /// mHz, as the protocol reports it.
    pub refresh: i32,
    pub preferred: bool,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", ModeSpec::from(*self))
    }
}

/// A mode as written in the config: refresh is optional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeSpec {
    pub width: i32,
    pub height: i32,
    pub refresh: Option<i32>,
}

impl From<Mode> for ModeSpec {
    fn from(m: Mode) -> Self {
        Self { width: m.width, height: m.height, refresh: Some(m.refresh) }
    }
}

impl fmt::Display for ModeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)?;
        if let Some(refresh) = self.refresh {
            write!(f, "@{:.3}", refresh as f64 / 1000.0)?;
        }
        Ok(())
    }
}

impl FromStr for ModeSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (size, refresh) = match s.split_once('@') {
            Some((size, rate)) => {
                let rate = rate.trim_end_matches("Hz").trim_end_matches("hz").trim();
                let hz: f64 = rate.parse().with_context(|| format!("bad refresh rate {rate:?}"))?;
                (size, Some((hz * 1000.0).round() as i32))
            }
            None => (s, None),
        };
        let (w, h) = size.split_once('x').with_context(|| format!("bad mode {s:?}"))?;
        Ok(Self {
            width: w.trim().parse().with_context(|| format!("bad mode width in {s:?}"))?,
            height: h.trim().parse().with_context(|| format!("bad mode height in {s:?}"))?,
            refresh,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Head {
    pub id: HeadId,
    pub description: String,
    pub modes: Vec<Mode>,
    pub enabled: bool,
    pub current_mode: Option<Mode>,
    pub position: (i32, i32),
    pub scale: f64,
    pub transform: Transform,
}

impl Head {
    /// Size the output occupies in the global coordinate space, i.e. mode divided
    /// by scale, with the axes swapped for a quarter turn.
    pub fn logical_size(&self) -> (i32, i32) {
        let mode = self.current_mode.or_else(|| self.modes.iter().copied().find(|m| m.preferred));
        let (w, h) = mode.map_or((1920, 1080), |m| (m.width, m.height));
        let (w, h) = if rotated(self.transform) { (h, w) } else { (w, h) };
        logical_size(w, h, self.scale)
    }
}

pub fn rotated(transform: Transform) -> bool {
    matches!(transform, Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270)
}

/// The scale the compositor will really use: wlroots snaps it to 120ths, the
/// increment the fractional-scale protocol carries. The wire value needs it too —
/// scale travels as wl_fixed, so a configured 1.6 is reported back as 1.6015625.
pub fn quantized_scale(scale: f64) -> f64 {
    let steps = if scale > 0.0 { (scale * 120.0).round() } else { 120.0 };
    steps.max(1.0) / 120.0
}

/// The logical size wlroots derives from a mode and a scale: 120ths arithmetic,
/// truncated. Derived any other way it comes out a pixel or three off, and then
/// outputs placed edge to edge really overlap — a strip of one screen shows up on
/// the other.
pub fn logical_size(width: i32, height: i32, scale: f64) -> (i32, i32) {
    let steps = (quantized_scale(scale) * 120.0) as i64;
    let logical = |px: i32| (px as i64 * 120 / steps) as i32;
    (logical(width), logical(height))
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub serial: u32,
    pub heads: Vec<Head>,
}

impl Snapshot {
    pub fn head(&self, id: &HeadId) -> Option<&Head> {
        self.heads.iter().find(|h| &h.id == id)
    }
}

/// Desired state for one output, as handed to [`Wayland::apply`].
#[derive(Clone, Debug)]
pub struct OutputSetting {
    pub id: HeadId,
    pub enabled: bool,
    pub position: Option<(i32, i32)>,
    pub scale: Option<f64>,
    pub mode: Option<ModeSpec>,
    pub transform: Option<Transform>,
}

impl OutputSetting {
    pub fn from_head(head: &Head) -> Self {
        Self {
            id: head.id.clone(),
            enabled: head.enabled,
            position: Some(head.position),
            scale: Some(head.scale),
            mode: head.current_mode.map(ModeSpec::from),
            transform: Some(head.transform),
        }
    }
}

pub fn transform_name(t: Transform) -> &'static str {
    match t {
        Transform::_90 => "90",
        Transform::_180 => "180",
        Transform::_270 => "270",
        Transform::Flipped => "flipped",
        Transform::Flipped90 => "flipped-90",
        Transform::Flipped180 => "flipped-180",
        Transform::Flipped270 => "flipped-270",
        _ => "normal",
    }
}

pub fn transform_from_name(s: &str) -> Result<Transform> {
    Ok(match s {
        "normal" | "0" => Transform::Normal,
        "90" => Transform::_90,
        "180" => Transform::_180,
        "270" => Transform::_270,
        "flipped" => Transform::Flipped,
        "flipped-90" => Transform::Flipped90,
        "flipped-180" => Transform::Flipped180,
        "flipped-270" => Transform::Flipped270,
        other => bail!("unknown transform {other:?}"),
    })
}

/// All transforms, in the order the UI offers them.
pub const TRANSFORMS: [Transform; 8] = [
    Transform::Normal,
    Transform::_90,
    Transform::_180,
    Transform::_270,
    Transform::Flipped,
    Transform::Flipped90,
    Transform::Flipped180,
    Transform::Flipped270,
];

#[derive(Debug, Default)]
struct RawMode {
    width: i32,
    height: i32,
    refresh: i32,
    preferred: bool,
}

#[derive(Debug)]
struct RawHead {
    proxy: ZwlrOutputHeadV1,
    name: String,
    description: String,
    make: String,
    model: String,
    serial: String,
    enabled: bool,
    current_mode: Option<ObjectId>,
    position: (i32, i32),
    scale: f64,
    transform: Transform,
    modes: Vec<ObjectId>,
}

impl RawHead {
    fn new(proxy: ZwlrOutputHeadV1) -> Self {
        Self {
            proxy,
            name: String::new(),
            description: String::new(),
            make: UNKNOWN.to_string(),
            model: UNKNOWN.to_string(),
            serial: UNKNOWN.to_string(),
            enabled: false,
            current_mode: None,
            position: (0, 0),
            scale: 1.0,
            transform: Transform::Normal,
            modes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Succeeded,
    Failed,
    /// The serial went stale because the output state changed underneath us.
    Cancelled,
}

/// Accumulated output-manager state. Also the calloop loop data, so the daemon can
/// drive it from an event loop.
#[derive(Debug, Default)]
pub struct OutputState {
    manager: Option<ZwlrOutputManagerV1>,
    head_order: Vec<ObjectId>,
    heads: HashMap<ObjectId, RawHead>,
    modes: HashMap<ObjectId, RawMode>,
    mode_proxies: HashMap<ObjectId, ZwlrOutputModeV1>,
    serial: u32,
    /// Bumped on every `done` event, i.e. once per complete output-state update.
    pub generation: u64,
    pending_config: Option<ZwlrOutputConfigurationV1>,
    apply_outcome: Option<ApplyOutcome>,
}

impl OutputState {
    pub fn snapshot(&self) -> Snapshot {
        let heads = self
            .head_order
            .iter()
            .filter_map(|id| self.heads.get(id))
            .map(|h| {
                let modes: Vec<Mode> = h.modes.iter().filter_map(|m| self.mode(m)).collect();
                Head {
                    id: HeadId {
                        connector: h.name.clone(),
                        make: h.make.clone(),
                        model: h.model.clone(),
                        serial: h.serial.clone(),
                    },
                    description: h.description.clone(),
                    modes,
                    enabled: h.enabled,
                    current_mode: h.current_mode.as_ref().and_then(|m| self.mode(m)),
                    position: h.position,
                    scale: h.scale,
                    transform: h.transform,
                }
            })
            .collect();
        Snapshot { serial: self.serial, heads }
    }

    fn mode(&self, id: &ObjectId) -> Option<Mode> {
        self.modes.get(id).map(|m| Mode {
            width: m.width,
            height: m.height,
            refresh: m.refresh,
            preferred: m.preferred,
        })
    }

    fn head_by_id(&self, id: &HeadId) -> Option<&RawHead> {
        self.head_order
            .iter()
            .filter_map(|oid| self.heads.get(oid))
            .find(|h| h.name == id.connector && h.make == id.make && h.model == id.model && h.serial == id.serial)
    }

    /// The advertised mode matching a spec, preferring an exact refresh match and
    /// otherwise the highest refresh at that resolution.
    fn mode_proxy(&self, head: &RawHead, spec: &ModeSpec) -> Option<ZwlrOutputModeV1> {
        let mut best: Option<(&ObjectId, &RawMode)> = None;
        for oid in &head.modes {
            let Some(m) = self.modes.get(oid) else { continue };
            if m.width != spec.width || m.height != spec.height {
                continue;
            }
            if let Some(refresh) = spec.refresh {
                // Refresh rates round-trip through a 3-decimal Hz string.
                if (m.refresh - refresh).abs() <= 1 {
                    return self.mode_proxies.get(oid).cloned();
                }
            }
            if best.is_none_or(|(_, b)| m.refresh > b.refresh) {
                best = Some((oid, m));
            }
        }
        best.and_then(|(oid, _)| self.mode_proxies.get(oid).cloned())
    }

    /// Send a configuration to the compositor without waiting for the result.
    ///
    /// Outputs not named in `settings` keep their current configuration: the
    /// protocol requires every head to be part of a configuration, so the current
    /// state is the starting point.
    pub fn configure(&mut self, qh: &QueueHandle<Self>, settings: &[OutputSetting]) -> Result<()> {
        let manager = self.manager.clone().context("output manager is gone")?;
        let snapshot = self.snapshot();

        let mut plan: Vec<OutputSetting> = snapshot.heads.iter().map(OutputSetting::from_head).collect();
        for want in settings {
            let slot = plan
                .iter_mut()
                .find(|p| p.id == want.id)
                .with_context(|| format!("output {} is not connected", want.id))?;
            *slot = want.clone();
        }

        let config = manager.create_configuration(self.serial, qh, ());
        for want in &plan {
            let head = self.head_by_id(&want.id).context("output vanished mid-apply")?;
            let proxy = head.proxy.clone();
            if !want.enabled {
                config.disable_head(&proxy);
                continue;
            }
            let mode = want.mode.and_then(|spec| self.mode_proxy(head, &spec));
            let head_config = config.enable_head(&proxy, qh, ());
            match (want.mode, mode) {
                (_, Some(proxy)) => head_config.set_mode(&proxy),
                (Some(spec), None) => head_config.set_custom_mode(spec.width, spec.height, spec.refresh.unwrap_or(0)),
                (None, None) => {}
            }
            if let Some((x, y)) = want.position {
                head_config.set_position(x, y);
            }
            if let Some(scale) = want.scale {
                head_config.set_scale(scale);
            }
            if let Some(transform) = want.transform {
                head_config.set_transform(transform);
            }
        }
        config.apply();

        if let Some(previous) = self.pending_config.replace(config) {
            previous.destroy();
        }
        self.apply_outcome = None;
        Ok(())
    }

    /// The result of the configuration sent by [`OutputState::configure`], once
    /// the compositor has answered.
    pub fn take_outcome(&mut self) -> Option<ApplyOutcome> {
        let outcome = self.apply_outcome.take()?;
        if let Some(config) = self.pending_config.take() {
            config.destroy();
        }
        Some(outcome)
    }

    fn forget_head(&mut self, id: &ObjectId) {
        if let Some(head) = self.heads.remove(id) {
            for mode in &head.modes {
                self.modes.remove(mode);
                if let Some(proxy) = self.mode_proxies.remove(mode) {
                    proxy.release();
                }
            }
            head.proxy.release();
        }
        self.head_order.retain(|h| h != id);
    }
}

/// A connection to the compositor's output manager.
pub struct Wayland {
    pub conn: Connection,
    pub queue: EventQueue<OutputState>,
    pub state: OutputState,
}

impl Wayland {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("cannot connect to the Wayland display")?;
        let mut queue = conn.new_event_queue();
        conn.display().get_registry(&queue.handle(), ());

        let mut state = OutputState::default();
        queue.roundtrip(&mut state)?;
        if state.manager.is_none() {
            bail!("compositor does not support wlr-output-management-unstable-v1");
        }
        while state.generation == 0 {
            queue.blocking_dispatch(&mut state)?;
        }
        Ok(Self { conn, queue, state })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.snapshot()
    }

    /// Dispatch until the compositor has sent a fresh complete output state.
    pub fn refresh(&mut self) -> Result<()> {
        let generation = self.state.generation;
        while self.state.generation == generation {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        Ok(())
    }

    /// Apply `settings` and block until the compositor has answered. Outputs not
    /// mentioned keep their current configuration.
    pub fn apply(&mut self, settings: &[OutputSetting]) -> Result<()> {
        for _ in 0..2 {
            let qh = self.queue.handle();
            self.state.configure(&qh, settings)?;
            let outcome = loop {
                self.queue.blocking_dispatch(&mut self.state)?;
                if let Some(outcome) = self.state.take_outcome() {
                    break outcome;
                }
            };
            match outcome {
                ApplyOutcome::Succeeded => return Ok(()),
                ApplyOutcome::Failed => bail!("compositor rejected the configuration"),
                // Stale serial: pick up the new output state and try once more.
                ApplyOutcome::Cancelled => self.refresh()?,
            }
        }
        bail!("configuration cancelled twice, output state keeps changing")
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for OutputState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == ZwlrOutputManagerV1::interface().name
            && state.manager.is_none()
        {
            let version = version.min(MANAGER_VERSION);
            state.manager = Some(registry.bind::<ZwlrOutputManagerV1, _, _>(name, version, qh, ()));
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for OutputState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                state.head_order.push(head.id());
                state.heads.insert(head.id(), RawHead::new(head));
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                state.serial = serial;
                state.generation += 1;
            }
            zwlr_output_manager_v1::Event::Finished => state.manager = None,
            _ => {}
        }
    }

    event_created_child!(OutputState, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for OutputState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_head_v1::Event;

        if let Event::Finished = event {
            state.forget_head(&proxy.id());
            return;
        }
        if let Event::Mode { mode } = event {
            let id = mode.id();
            state.modes.insert(id.clone(), RawMode::default());
            state.mode_proxies.insert(id.clone(), mode);
            if let Some(head) = state.heads.get_mut(&proxy.id()) {
                head.modes.push(id);
            }
            return;
        }

        let Some(head) = state.heads.get_mut(&proxy.id()) else { return };
        match event {
            Event::Name { name } => head.name = name,
            Event::Description { description } => head.description = description,
            Event::Make { make } => head.make = make,
            Event::Model { model } => head.model = model,
            Event::SerialNumber { serial_number } => head.serial = serial_number,
            Event::Enabled { enabled } => head.enabled = enabled != 0,
            Event::CurrentMode { mode } => head.current_mode = Some(mode.id()),
            Event::Position { x, y } => head.position = (x, y),
            Event::Scale { scale } => head.scale = scale,
            Event::Transform { transform: WEnum::Value(transform) } => head.transform = transform,
            _ => {}
        }
    }

    event_created_child!(OutputState, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for OutputState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(mode) = state.modes.get_mut(&proxy.id()) else { return };
        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                mode.width = width;
                mode.height = height;
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => mode.refresh = refresh,
            zwlr_output_mode_v1::Event::Preferred => mode.preferred = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for OutputState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.apply_outcome = Some(match event {
            zwlr_output_configuration_v1::Event::Succeeded => ApplyOutcome::Succeeded,
            zwlr_output_configuration_v1::Event::Failed => ApplyOutcome::Failed,
            zwlr_output_configuration_v1::Event::Cancelled => ApplyOutcome::Cancelled,
            _ => return,
        });
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for OutputState {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: <ZwlrOutputConfigurationHeadV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_spec_round_trip() {
        let spec: ModeSpec = "3840x2160@60.000".parse().unwrap();
        assert_eq!(spec, ModeSpec { width: 3840, height: 2160, refresh: Some(60_000) });
        assert_eq!(spec.to_string(), "3840x2160@60.000");

        let spec: ModeSpec = "1920x1080".parse().unwrap();
        assert_eq!(spec, ModeSpec { width: 1920, height: 1080, refresh: None });
        assert_eq!(spec.to_string(), "1920x1080");

        assert_eq!("2560x1440@59.951Hz".parse::<ModeSpec>().unwrap().refresh, Some(59_951));
        assert!("nonsense".parse::<ModeSpec>().is_err());
    }

    #[test]
    fn transform_names_round_trip() {
        for t in TRANSFORMS {
            assert_eq!(transform_from_name(transform_name(t)).unwrap(), t);
        }
    }

    #[test]
    fn logical_size_accounts_for_scale_and_rotation() {
        let head = Head {
            id: HeadId::default(),
            description: String::new(),
            modes: vec![],
            enabled: true,
            current_mode: Some(Mode { width: 3840, height: 2160, refresh: 60_000, preferred: true }),
            position: (0, 0),
            scale: 1.6,
            transform: Transform::Normal,
        };
        assert_eq!(head.logical_size(), (2400, 1350));

        let rotated = Head { transform: Transform::_270, scale: 1.0, ..head };
        assert_eq!(rotated.logical_size(), (2160, 3840));
    }

    /// Measured against sway: the scale is snapped to 120ths and the division
    /// truncated, so a screen 3px narrower than the compositor thinks — which is
    /// what `round(px / scale)` gives for 3840 at 1.6 over the wire — would make
    /// the next screen overlap it.
    #[test]
    fn logical_size_matches_the_compositor() {
        assert_eq!(logical_size(1280, 720, 1.45), (882, 496));
        assert_eq!(logical_size(1280, 720, 1.3), (984, 553));
        assert_eq!(logical_size(1280, 720, 1.7), (752, 423));
        assert_eq!(logical_size(3840, 2160, 1.6), (2400, 1350));
        // wl_fixed reports a configured 1.6 as 1.6015625; same output.
        assert_eq!(logical_size(3840, 2160, 1.6015625), (2400, 1350));
        assert_eq!(quantized_scale(1.6015625), 1.6);
    }
}

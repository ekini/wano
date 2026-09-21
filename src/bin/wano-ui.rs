//! Visual arranger: drag the outputs into place, set scale/mode/transform, apply
//! it live, save it as a profile. Talks to the compositor on its own connection,
//! so it does not need the daemon.

use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::{Context as _, Result};
use calloop::{EventLoop, channel};
use calloop_wayland_source::WaylandSource;
use clap::Parser;
use eframe::egui;
use wayland_client::protocol::wl_output::Transform;

use wano::config::{Config, Output, Profile, suggested_name};
use wano::matching::{best_match, describes, match_profile};
use wano::wl::{
    ApplyOutcome, Head, HeadId, ModeSpec, OutputSetting, OutputState, Snapshot, TRANSFORMS, Wayland, logical_size,
    quantized_scale, rotated, transform_from_name, transform_name,
};

/// How near an alignment a drop has to land, in screen pixels, for the output to
/// stick to it.
const STICKY: f32 = 16.0;

/// Half-life of the slide into the settled position: the magnet pulls the output
/// in over roughly four of these.
const MAGNET_HALF_LIFE: f32 = 0.1;

/// Amber for the snap preview: legible on either theme, and not a colour the rest
/// of the canvas uses.
const STICKY_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 176, 0);

#[derive(Parser)]
#[command(about = "Visual monitor arranger for wlroots compositors")]
struct Args {
    /// Profile file to save into
    #[arg(short, long)]
    config: Option<PathBuf>,
}

enum Request {
    Apply(Vec<OutputSetting>),
}

enum Update {
    Heads(Vec<Head>),
    Outcome(ApplyOutcome),
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).format_timestamp(None).init();
    let args = Args::parse();
    let path = args.config.unwrap_or_else(Config::path);

    let wl = Wayland::connect()?;
    let heads = wl.snapshot().heads;
    let (request_tx, request_rx) = channel::channel::<Request>();
    let (update_tx, update_rx) = mpsc::channel::<Update>();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 600.0]).with_app_id("wano-ui"),
        ..Default::default()
    };
    eframe::run_native(
        "wano",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let tx = update_tx.clone();
            std::thread::Builder::new()
                .name("wayland".into())
                .spawn(move || {
                    if let Err(e) = worker(wl, ctx, request_rx, tx) {
                        log::error!("wayland thread stopped: {e:#}");
                    }
                })
                .expect("spawning the wayland thread");
            Ok(Box::new(App::new(path, heads, request_tx, update_rx)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Owns the Wayland connection: pushes output state to the GUI, applies what it
/// sends back.
fn worker(
    wl: Wayland,
    ctx: egui::Context,
    requests: channel::Channel<Request>,
    updates: mpsc::Sender<Update>,
) -> Result<()> {
    let Wayland { conn, queue, mut state } = wl;
    let qh = queue.handle();

    let mut event_loop: EventLoop<OutputState> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, queue).insert(handle.clone()).map_err(|e| e.error)?;
    handle
        .insert_source(requests, move |event, _, state: &mut OutputState| {
            if let channel::Event::Msg(Request::Apply(settings)) = event
                && let Err(e) = state.configure(&qh, &settings)
            {
                log::error!("cannot apply: {e:#}");
            }
        })
        .map_err(|e| e.error)?;

    let mut generation = state.generation;
    loop {
        event_loop.dispatch(None, &mut state).context("event loop failed")?;
        let mut changed = false;
        if state.generation != generation {
            generation = state.generation;
            changed |= updates.send(Update::Heads(state.snapshot().heads)).is_ok();
        }
        if let Some(outcome) = state.take_outcome() {
            changed |= updates.send(Update::Outcome(outcome)).is_ok();
        }
        if changed {
            ctx.request_repaint();
        }
    }
}

/// The editable state of one output. Positions stay floating point so a drag does
/// not accumulate rounding error.
#[derive(Clone)]
struct Draft {
    id: HeadId,
    /// The profile entry this came from, when editing a saved profile rather
    /// than the live outputs: its match keys are kept as they were written.
    keys: Option<Output>,
    enabled: bool,
    x: f32,
    y: f32,
    scale: f64,
    mode: Option<ModeSpec>,
    transform: Transform,
}

impl Draft {
    fn from_head(head: &Head) -> Self {
        Self {
            id: head.id.clone(),
            keys: None,
            enabled: head.enabled,
            x: head.position.0 as f32,
            y: head.position.1 as f32,
            scale: quantized_scale(head.scale),
            mode: head.current_mode.or_else(|| head.modes.iter().copied().find(|m| m.preferred)).map(ModeSpec::from),
            transform: head.transform,
        }
    }

    fn from_output(output: &Output) -> Self {
        let [x, y] = output.position.unwrap_or([0, 0]);
        let field = |f: &Option<String>| f.clone().unwrap_or_default();
        Self {
            id: HeadId {
                connector: field(&output.connector),
                make: field(&output.make),
                model: field(&output.model),
                serial: field(&output.serial),
            },
            keys: Some(output.clone()),
            enabled: output.enabled,
            x: x as f32,
            y: y as f32,
            scale: output.scale.unwrap_or(1.0),
            mode: output.mode.as_deref().and_then(|m| m.parse().ok()),
            transform: output
                .transform
                .as_deref()
                .and_then(|t| transform_from_name(t).ok())
                .unwrap_or(Transform::Normal),
        }
    }

    fn to_output(&self, heads: &[Head], with_serial: bool) -> Output {
        let keys = match &self.keys {
            Some(k) => Output { serial: k.serial.clone().filter(|_| with_serial), ..k.clone() },
            None => heads
                .iter()
                .find(|h| h.id == self.id)
                .map_or_else(Output::default, |h| Output::keys(&h.id, with_serial)),
        };
        Output {
            enabled: self.enabled,
            position: Some([self.x.round() as i32, self.y.round() as i32]),
            scale: Some((self.scale * 1000.0).round() / 1000.0),
            mode: self.mode.map(|m| m.to_string()),
            transform: Some(transform_name(self.transform).to_string()),
            ..keys
        }
    }

    /// The live head this output stands for, if one is connected.
    fn head<'a>(&self, heads: &'a [Head]) -> Option<&'a Head> {
        match &self.keys {
            Some(k) => heads.iter().find(|h| describes(k, h)),
            None => heads.iter().find(|h| h.id == self.id),
        }
    }

    fn short_name(&self) -> &str {
        if self.id.connector.is_empty() { &self.id.model } else { &self.id.connector }
    }

    fn name(&self) -> String {
        let parts = [&self.id.connector, &self.id.model];
        parts.into_iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" — ")
    }

    fn label(&self) -> String {
        self.keys.as_ref().map_or_else(|| self.id.to_string(), Output::label)
    }

    fn size(&self) -> egui::Vec2 {
        let (w, h) = self.mode.map_or((1920, 1080), |m| (m.width, m.height));
        let (w, h) = if rotated(self.transform) { (h, w) } else { (w, h) };
        // Exactly the compositor's own logical size, so outputs placed edge to
        // edge really are edge to edge: a footprint a few pixels out puts a strip
        // of one screen on the other.
        let (w, h) = logical_size(w, h, self.scale);
        egui::vec2(w as f32, h as f32)
    }

    fn setting(&self) -> OutputSetting {
        OutputSetting {
            id: self.id.clone(),
            enabled: self.enabled,
            position: Some((self.x.round() as i32, self.y.round() as i32)),
            scale: Some(self.scale),
            mode: self.mode,
            transform: Some(self.transform),
        }
    }
}

struct App {
    path: PathBuf,
    requests: channel::Sender<Request>,
    updates: mpsc::Receiver<Update>,
    heads: Vec<Head>,
    /// State at launch, for Revert.
    base: Vec<Draft>,
    drafts: Vec<Draft>,
    selected: usize,
    /// Zoom and pan of the canvas, held still while something is being dragged.
    view: Option<(f32, egui::Vec2)>,
    /// Per output, how far it is still drawn from its real position: the tail of
    /// the slide into the settled layout.
    slides: Vec<egui::Vec2>,
    name: String,
    with_serial: bool,
    status: String,
    config: Config,
    /// Name of the saved profile being edited, when the drafts are not the
    /// live outputs.
    editing: Option<String>,
}

impl App {
    fn new(
        path: PathBuf,
        heads: Vec<Head>,
        requests: channel::Sender<Request>,
        updates: mpsc::Receiver<Update>,
    ) -> Self {
        let drafts: Vec<Draft> = heads.iter().map(Draft::from_head).collect();
        let config = Config::load(&path).unwrap_or_default();
        let name = best_match(&config, &heads)
            .map(|m| m.profile.name.clone())
            .unwrap_or_else(|| suggested_name(&Snapshot { serial: 0, heads: heads.clone() }));
        Self {
            path,
            requests,
            updates,
            heads,
            base: drafts.clone(),
            drafts,
            selected: 0,
            view: None,
            slides: Vec::new(),
            name,
            with_serial: true,
            status: String::new(),
            config,
            editing: None,
        }
    }

    fn reload(&mut self) {
        self.config = Config::load(&self.path).unwrap_or_default();
    }

    fn load(&mut self, drafts: Vec<Draft>) {
        self.base = drafts.clone();
        self.drafts = drafts;
        self.slides.clear();
        self.selected = 0;
        self.view = None;
    }

    /// Edit a saved profile in place of the live outputs.
    fn edit(&mut self, name: &str) {
        let Some(profile) = self.config.profile(name) else { return };
        self.name = profile.name.clone();
        self.with_serial = profile.outputs.iter().any(|o| o.serial.is_some());
        self.editing = Some(profile.name.clone());
        self.load(profile.outputs.iter().map(Draft::from_output).collect());
    }

    fn edit_live(&mut self) {
        self.editing = None;
        self.name = best_match(&self.config, &self.heads)
            .map(|m| m.profile.name.clone())
            .unwrap_or_else(|| suggested_name(&Snapshot { serial: 0, heads: self.heads.clone() }));
        self.with_serial = true;
        self.load(self.heads.iter().map(Draft::from_head).collect());
    }

    fn drain(&mut self) {
        while let Ok(update) = self.updates.try_recv() {
            match update {
                Update::Heads(heads) => {
                    // Keep the edits in progress unless the outputs themselves changed.
                    let same: Vec<&HeadId> = heads.iter().map(|h| &h.id).collect();
                    if self.editing.is_none()
                        && (self.drafts.len() != heads.len() || !self.drafts.iter().all(|d| same.contains(&&d.id)))
                    {
                        self.drafts = heads.iter().map(Draft::from_head).collect();
                        self.base = self.drafts.clone();
                        self.slides.clear();
                        self.selected = 0;
                    }
                    self.heads = heads;
                }
                Update::Outcome(ApplyOutcome::Succeeded) => self.status = "applied".into(),
                Update::Outcome(ApplyOutcome::Failed) => self.status = "the compositor rejected it".into(),
                Update::Outcome(ApplyOutcome::Cancelled) => self.status = "cancelled, outputs changed".into(),
            }
        }
    }

    fn apply(&mut self) {
        // A saved profile reaches the outputs the same way the daemon applies it:
        // through its match keys, so it only applies where it fits.
        let settings: Result<Vec<OutputSetting>> = if self.editing.is_some() {
            match match_profile(&self.profile(), &self.heads) {
                Some(m) => m.settings(&self.heads),
                None => Err(anyhow::anyhow!("this profile does not describe the connected outputs")),
            }
        } else {
            Ok(self.drafts.iter().map(Draft::setting).collect())
        };
        match settings {
            Ok(settings) => {
                if self.requests.send(Request::Apply(settings)).is_err() {
                    self.status = "the wayland connection is gone".into();
                }
            }
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    fn save(&mut self) {
        self.status = match self.write_profile() {
            Ok(()) => format!("saved profile {} to {}", self.name, self.path.display()),
            Err(e) => format!("{e:#}"),
        };
        self.reload();
    }

    fn delete(&mut self, name: &str) {
        let mut config = self.config.clone();
        config.remove(name);
        self.status = match config.save(&self.path) {
            Ok(()) => format!("deleted profile {name}"),
            Err(e) => format!("{e:#}"),
        };
        self.reload();
        if self.editing.as_deref() == Some(name) {
            self.edit_live();
        }
    }

    fn profile(&self) -> Profile {
        Profile {
            name: self.name.trim().to_string(),
            outputs: self.drafts.iter().map(|d| d.to_output(&self.heads, self.with_serial)).collect(),
        }
    }

    fn write_profile(&mut self) -> Result<()> {
        if self.name.trim().is_empty() {
            anyhow::bail!("the profile needs a name");
        }
        let profile = self.profile();
        let mut config = Config::load(&self.path)?;
        // Saving an edited profile under a new name renames it.
        if let Some(old) = &self.editing {
            if *old != profile.name {
                config.remove(old);
            }
            self.editing = Some(profile.name.clone());
        }
        config.upsert(profile);
        config.save(&self.path)
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Outputs");
        for (i, draft) in self.drafts.iter().enumerate() {
            ui.selectable_value(&mut self.selected, i, draft.name());
        }
        ui.separator();

        let mut changed = false;
        if let Some(draft) = self.drafts.get_mut(self.selected) {
            let head = draft.head(&self.heads);
            ui.label(draft.label());
            changed |= ui.checkbox(&mut draft.enabled, "Enabled").changed();

            ui.horizontal(|ui| {
                ui.label("Scale");
                changed |= ui
                    .add(egui::DragValue::new(&mut draft.scale).speed(0.05).range(0.5..=4.0).max_decimals(3))
                    .changed();
            });

            let modes = head.map(|h| h.modes.clone()).unwrap_or_default();
            changed |= egui::ComboBox::from_label("Mode")
                .selected_text(draft.mode.map_or_else(|| "—".to_string(), |m| m.to_string()))
                .show_ui(ui, |ui| {
                    let mut picked = false;
                    for mode in &modes {
                        let label = if mode.preferred { format!("{mode} *") } else { mode.to_string() };
                        picked |= ui.selectable_value(&mut draft.mode, Some(ModeSpec::from(*mode)), label).changed();
                    }
                    picked
                })
                .inner
                .unwrap_or(false);

            changed |= egui::ComboBox::from_label("Transform")
                .selected_text(transform_name(draft.transform))
                .show_ui(ui, |ui| {
                    let mut picked = false;
                    for transform in TRANSFORMS {
                        picked |=
                            ui.selectable_value(&mut draft.transform, transform, transform_name(transform)).changed();
                    }
                    picked
                })
                .inner
                .unwrap_or(false);

            ui.horizontal(|ui| {
                ui.label("Position");
                changed |= ui.add(egui::DragValue::new(&mut draft.x).speed(1.0).max_decimals(0).prefix("x ")).changed();
                changed |= ui.add(egui::DragValue::new(&mut draft.y).speed(1.0).max_decimals(0).prefix("y ")).changed();
            });
        }
        if changed {
            self.settle_from(self.selected);
        }

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                self.apply();
            }
            if ui.button("Revert").clicked() {
                self.drafts = self.base.clone();
                self.apply();
            }
        });

        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Profile");
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(140.0));
        });
        ui.checkbox(&mut self.with_serial, "Match this exact monitor (serial)");
        if ui.button("Save profile").clicked() {
            self.save();
        }

        ui.separator();
        ui.heading("Profiles");
        if self.config.profiles.is_empty() {
            ui.weak("no profiles yet");
        }
        let mut open = None;
        let mut delete = None;
        for profile in &self.config.profiles {
            ui.horizontal(|ui| {
                if ui.selectable_label(self.editing.as_deref() == Some(&profile.name), &profile.name).clicked() {
                    open = Some(profile.name.clone());
                }
                if match_profile(profile, &self.heads).is_some() {
                    ui.weak("✓").on_hover_text("matches the connected outputs");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("🗑").on_hover_text("delete").clicked() {
                        delete = Some(profile.name.clone());
                    }
                });
            });
        }
        if let Some(name) = open {
            self.edit(&name);
        }
        if let Some(name) = delete {
            self.delete(&name);
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        if let Some(name) = self.editing.clone() {
            ui.horizontal(|ui| {
                ui.weak(format!("editing profile {name}"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Back to live").clicked() {
                        self.edit_live();
                    }
                });
            });
        }
        let area = ui.available_rect_before_wrap();
        let painter = ui.painter_at(area);

        let Some(bounds) = self.bounds() else {
            painter.text(
                area.center(),
                egui::Align2::CENTER_CENTER,
                "every output is disabled",
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            return;
        };
        self.slides.resize(self.drafts.len(), egui::Vec2::ZERO);
        let sliding = self.slides.iter().any(|&s| s != egui::Vec2::ZERO);

        // Refitting the canvas mid-drag would slide the rectangle out from under
        // the pointer, so the view only follows the layout when nothing is moving.
        let (zoom, offset) =
            self.view.filter(|_| ui.ctx().dragged_id().is_some() || sliding).unwrap_or_else(|| fit(area, bounds));
        self.view = Some((zoom, offset));
        let to_screen = |p: egui::Pos2| (p.to_vec2() * zoom + offset).to_pos2();

        let visuals = ui.visuals().clone();
        let mut dragged = None;
        let mut dropped = None;
        for i in 0..self.drafts.len() {
            if !self.drafts[i].enabled {
                continue;
            }
            let draft = &self.drafts[i];
            let at = egui::pos2(draft.x, draft.y) + self.slides[i];
            let rect = egui::Rect::from_min_size(to_screen(at), draft.size() * zoom);
            let response = ui.interact(rect, egui::Id::new(("output", i)), egui::Sense::click_and_drag());
            if response.clicked() || response.drag_started() {
                self.selected = i;
            }
            // The rectangle follows the pointer freely, gaps and all; it snaps
            // against its neighbours once dropped.
            if response.dragged() {
                let delta = response.drag_delta() / zoom;
                self.drafts[i].x += delta.x;
                self.drafts[i].y += delta.y;
                dragged = Some(i);
            }
            if response.drag_stopped() {
                dropped = Some(i);
            }

            let selected = self.selected == i;
            let fill = if selected { visuals.selection.bg_fill } else { visuals.widgets.inactive.bg_fill };
            painter.rect_filled(rect, 4, fill);
            painter.rect_stroke(
                rect,
                4,
                egui::Stroke::new(if selected { 2.0 } else { 1.0 }, visuals.widgets.active.fg_stroke.color),
                egui::StrokeKind::Inside,
            );
            let draft = &self.drafts[i];
            let size = draft.size();
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("{}\n{}×{} @ {}", draft.short_name(), size.x as i32, size.y as i32, draft.scale),
                egui::FontId::proportional(13.0),
                visuals.strong_text_color(),
            );
        }

        // While dragging, show the outline the drop will land in and the
        // alignments it will land on.
        if let Some(i) = dragged {
            let mut preview = self.drafts.clone();
            settle(&mut preview, i, STICKY / zoom);
            let target = rect_of(&preview[i]);
            let others: Vec<egui::Rect> =
                self.drafts.iter().enumerate().filter(|&(j, d)| j != i && d.enabled).map(|(_, d)| rect_of(d)).collect();
            let stroke = egui::Stroke::new(1.5, STICKY_COLOR);
            let screen = egui::Rect::from_min_max(to_screen(target.min), to_screen(target.max));
            let outline = [screen.left_top(), screen.right_top(), screen.right_bottom(), screen.left_bottom()];
            painter.extend(egui::Shape::dashed_line(&[outline.as_slice(), &outline[..1]].concat(), stroke, 8.0, 5.0));
            for [from, to] in guides(target, &others) {
                // A little past both ends, so the line reads as a guide rather than
                // as an edge of something.
                let (a, b) = (to_screen(from), to_screen(to));
                let over = (b - a).normalized() * 14.0;
                painter.extend(egui::Shape::dashed_line(&[a - over, b + over], stroke, 6.0, 4.0));
            }
        }

        if let Some(i) = dropped {
            self.settle_from(i);
        }

        // Ease the drawn position back onto the real one: the magnet pulling in.
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        let decay = 0.5f32.powf(dt / MAGNET_HALF_LIFE);
        let mut moving = false;
        for slide in &mut self.slides {
            *slide *= decay;
            if slide.length() * zoom < 0.5 {
                *slide = egui::Vec2::ZERO;
            } else {
                moving = true;
            }
        }
        if moving {
            ui.ctx().request_repaint();
        }
    }

    /// Settle the layout around output `i`, leaving every output that moved drawn
    /// where it was so it slides into place instead of jumping.
    fn settle_from(&mut self, i: usize) {
        let before: Vec<egui::Pos2> = self.drafts.iter().map(|d| egui::pos2(d.x, d.y)).collect();
        let sticky = self.sticky();
        settle(&mut self.drafts, i, sticky);
        self.slides.resize(self.drafts.len(), egui::Vec2::ZERO);
        for (j, from) in before.iter().enumerate() {
            self.slides[j] += *from - egui::pos2(self.drafts[j].x, self.drafts[j].y);
        }
    }

    /// The sticky distance in layout pixels, so it feels the same at any zoom.
    fn sticky(&self) -> f32 {
        self.view.map_or(STICKY, |(zoom, _)| STICKY / zoom)
    }

    /// Bounding box of the enabled outputs in global coordinates.
    fn bounds(&self) -> Option<egui::Rect> {
        let mut bounds: Option<egui::Rect> = None;
        for draft in self.drafts.iter().filter(|d| d.enabled) {
            let rect = egui::Rect::from_min_size(egui::pos2(draft.x, draft.y), draft.size());
            bounds = Some(bounds.map_or(rect, |b| b.union(rect)));
        }
        bounds
    }
}

/// The alignments `target` lands on: edges flush or centres in line with another
/// output, or with the block they form together.
fn guides(target: egui::Rect, others: &[egui::Rect]) -> Vec<[egui::Pos2; 2]> {
    let mut anchors = others.to_vec();
    if others.len() > 1 {
        anchors.extend(others.iter().copied().reduce(egui::Rect::union));
    }
    let mut lines: Vec<[egui::Pos2; 2]> = Vec::new();
    for anchor in anchors {
        let vertical =
            [(target.left(), anchor.left()), (target.center().x, anchor.center().x), (target.right(), anchor.right())];
        for (_, x) in vertical.into_iter().filter(|(a, b)| (a - b).abs() < 0.5) {
            let (top, bottom) = (target.top().min(anchor.top()), target.bottom().max(anchor.bottom()));
            lines.push([egui::pos2(x, top), egui::pos2(x, bottom)]);
        }
        let horizontal =
            [(target.top(), anchor.top()), (target.center().y, anchor.center().y), (target.bottom(), anchor.bottom())];
        for (_, b) in horizontal.into_iter().filter(|(a, b)| (a - b).abs() < 0.5) {
            let (left, right) = (target.left().min(anchor.left()), target.right().max(anchor.right()));
            lines.push([egui::pos2(left, b), egui::pos2(right, b)]);
        }
    }
    lines.dedup();
    lines
}

fn rect_of(draft: &Draft) -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(draft.x, draft.y), draft.size())
}

/// Zoom and pan that fit `bounds` inside `area` with a margin.
fn fit(area: egui::Rect, bounds: egui::Rect) -> (f32, egui::Vec2) {
    let margin = 24.0;
    let zoom = ((area.width() - 2.0 * margin) / bounds.width().max(1.0))
        .min((area.height() - 2.0 * margin) / bounds.height().max(1.0));
    (zoom, area.center().to_vec2() - bounds.center().to_vec2() * zoom)
}

/// Pull the enabled outputs together so the layout is gapless and free of
/// overlap: a gap is dead space the pointer cannot cross, and an overlap hides
/// part of a screen. `moved` is the output that was just dropped, so it goes
/// first and everything else keeps its position if it already fits.
fn settle(drafts: &mut [Draft], moved: usize, sticky: f32) {
    let mut rest: Vec<usize> = (0..drafts.len()).filter(|&i| i != moved && drafts[i].enabled).collect();
    if rest.is_empty() || !drafts[moved].enabled {
        return;
    }

    // The dropped output lands against the layout as a whole, which is what lets
    // it centre on the block the others form.
    place(drafts, moved, &rest, sticky);

    // Then whatever the move left dangling — an output the resize cut loose, or
    // one the drop displaced — is pulled back in, nearest first.
    let from = rect_of(&drafts[moved]).center();
    rest.sort_by_key(|&i| (rect_of(&drafts[i]).center() - from).length() as i32);
    let mut placed = vec![moved];
    for i in rest {
        if !fits(drafts, i, &placed) {
            place(drafts, i, &placed, sticky);
        }
        placed.push(i);
    }
}

fn place(drafts: &mut [Draft], i: usize, placed: &[usize], sticky: f32) {
    if let Some(pos) = flush_position(drafts, i, placed, sticky) {
        drafts[i].x = pos.x.round();
        drafts[i].y = pos.y.round();
    }
}

/// Whether output `i` already touches one of the outputs placed and overlaps none.
fn fits(drafts: &[Draft], i: usize, placed: &[usize]) -> bool {
    let a = rect_of(&drafts[i]);
    let mut touching = false;
    for b in placed.iter().map(|&p| rect_of(&drafts[p])) {
        if a.shrink(0.5).intersects(b.shrink(0.5)) {
            return false;
        }
        let flush = |x: f32, y: f32| (x - y).abs() < 0.5;
        touching |=
            (flush(a.right(), b.left()) || flush(b.right(), a.left())) && a.top() < b.bottom() && b.top() < a.bottom()
                || (flush(a.bottom(), b.top()) || flush(b.bottom(), a.top()))
                    && a.left() < b.right()
                    && b.left() < a.right();
    }
    touching
}

/// Where to put output `i` so it touches the outputs already placed without
/// overlapping any of them, moving it as little as possible. Dropping it within
/// `sticky` of an alignment — edges flush, centres in line, or centred on the
/// whole block of the others — lands on that alignment instead.
fn flush_position(drafts: &[Draft], i: usize, placed: &[usize], sticky: f32) -> Option<egui::Pos2> {
    let size = drafts[i].size();
    let current = egui::pos2(drafts[i].x, drafts[i].y);
    let rects: Vec<egui::Rect> = placed.iter().map(|&p| rect_of(&drafts[p])).collect();

    // Anchor on each output already placed, and on the block they form
    // together: that is what centres a screen under the row above it.
    let mut anchors = rects.clone();
    if rects.len() > 1 {
        anchors.extend(rects.iter().copied().reduce(egui::Rect::union));
    }

    let mut best: Option<(f32, egui::Pos2)> = None;
    for anchor in anchors {
        // Along the shared edge the output either keeps its own offset — clamped
        // so the edge really overlaps — or lines up with the anchor's near edge,
        // centre or far edge. Those three are the sticky points.
        let free_y = current.y.clamp(anchor.top() - size.y + 1.0, anchor.bottom() - 1.0);
        let free_x = current.x.clamp(anchor.left() - size.x + 1.0, anchor.right() - 1.0);
        let ys = [anchor.top(), anchor.center().y - size.y / 2.0, anchor.bottom() - size.y];
        let xs = [anchor.left(), anchor.center().x - size.x / 2.0, anchor.right() - size.x];

        let beside = [anchor.right(), anchor.left() - size.x];
        let over = [anchor.bottom(), anchor.top() - size.y];
        let candidates = beside
            .iter()
            .flat_map(|&x| std::iter::once((egui::pos2(x, free_y), false)).chain(ys.map(|y| (egui::pos2(x, y), true))))
            .chain(over.iter().flat_map(|&y| {
                std::iter::once((egui::pos2(free_x, y), false)).chain(xs.map(|x| (egui::pos2(x, y), true)))
            }));

        for (candidate, aligned) in candidates {
            let rect = egui::Rect::from_min_size(candidate, size).shrink(0.5);
            if rects.iter().any(|other| other.shrink(0.5).intersects(rect)) {
                continue;
            }
            // An alignment is always the longer move, so it only ever wins with
            // a head start — which is exactly what makes it magnetic.
            let cost = (candidate - current).length() - if aligned { sticky } else { 0.0 };
            if best.is_none_or(|(lowest, _)| cost < lowest) {
                best = Some((cost, candidate));
            }
        }
    }
    best.map(|(_, pos)| pos)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        self.drain();

        egui::Panel::right("controls").exact_size(280.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.side_panel(ui));
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
            });
        });
        egui::CentralPanel::default().show(ui, |ui| self.canvas(ui));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(connector: &str, x: f32, y: f32) -> Draft {
        Draft {
            id: HeadId { connector: connector.into(), ..HeadId::default() },
            keys: None,
            enabled: true,
            x,
            y,
            scale: 1.0,
            mode: Some(ModeSpec { width: 1920, height: 1080, refresh: Some(60_000) }),
            transform: Transform::Normal,
        }
    }

    #[test]
    fn saving_from_the_live_layout_lists_the_new_profile() {
        let dir = std::env::temp_dir().join(format!("wano-ui-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let head = Head {
            id: HeadId {
                connector: "DP-1".into(),
                make: "Dell Inc.".into(),
                model: "DELL P2723QE".into(),
                serial: "X".into(),
            },
            description: String::new(),
            modes: vec![],
            enabled: true,
            current_mode: Some(wano::wl::Mode { width: 3840, height: 2160, refresh: 59_997, preferred: true }),
            position: (0, 0),
            scale: 1.6,
            transform: Transform::Normal,
        };
        let (tx, _rx) = channel::channel();
        let (_utx, urx) = mpsc::channel();
        let mut app = App::new(path.clone(), vec![head], tx, urx);
        app.name = "desk".into();
        app.save();
        assert_eq!(app.status, format!("saved profile desk to {}", path.display()));
        assert_eq!(app.config.profiles.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["desk"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_profile_entry_round_trips_through_a_draft() {
        let entry = Output {
            model: Some("PHL 241B7Q".into()),
            serial: Some("X".into()),
            enabled: true,
            position: Some([1920, 0]),
            scale: Some(1.5),
            mode: Some("2560x1440@59.951".into()),
            transform: Some("90".into()),
            ..Output::default()
        };
        let draft = Draft::from_output(&entry);
        assert_eq!(draft.size(), egui::vec2(960.0, 1706.0));
        let back = draft.to_output(&[], true);
        assert_eq!(back.model, entry.model);
        assert_eq!(back.serial, entry.serial);
        assert_eq!(back.position, entry.position);
        assert_eq!(back.mode, entry.mode);
        assert_eq!(back.transform, entry.transform);
        assert!(draft.to_output(&[], false).serial.is_none());
    }

    /// Every enabled output shares an edge with another one, and none overlap.
    fn contiguous(drafts: &[Draft]) -> bool {
        let rects: Vec<egui::Rect> = drafts.iter().filter(|d| d.enabled).map(rect_of).collect();
        let touches = |a: &egui::Rect, b: &egui::Rect| {
            let horizontal =
                (a.right() == b.left() || b.right() == a.left()) && a.top() < b.bottom() && b.top() < a.bottom();
            let vertical =
                (a.bottom() == b.top() || b.bottom() == a.top()) && a.left() < b.right() && b.left() < a.right();
            horizontal || vertical
        };
        rects.iter().enumerate().all(|(i, a)| {
            rects.iter().enumerate().any(|(j, b)| i != j && touches(a, b))
                && rects.iter().enumerate().all(|(j, b)| i == j || !a.shrink(0.5).intersects(b.shrink(0.5)))
        })
    }

    #[test]
    fn a_gap_is_closed_however_small() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 1928.0, 7.0)];
        settle(&mut drafts, 1, 0.0);
        assert_eq!((drafts[1].x, drafts[1].y), (1920.0, 7.0));
        assert!(contiguous(&drafts));
    }

    #[test]
    fn an_output_dropped_far_away_is_pulled_against_its_neighbour() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 6000.0, 4000.0)];
        settle(&mut drafts, 1, 16.0);
        assert!(contiguous(&drafts));
    }

    #[test]
    fn overlapping_outputs_are_pushed_apart() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 400.0, 100.0)];
        settle(&mut drafts, 1, 16.0);
        assert!(contiguous(&drafts));
    }

    #[test]
    fn a_layout_that_already_fits_is_left_alone() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 1920.0, 0.0), draft("eDP-1", 3840.0, 0.0)];
        let before: Vec<(f32, f32)> = drafts.iter().map(|d| (d.x, d.y)).collect();
        settle(&mut drafts, 1, 16.0);
        assert_eq!(drafts.iter().map(|d| (d.x, d.y)).collect::<Vec<_>>(), before);
    }

    #[test]
    fn rescaling_an_output_closes_the_gap_it_leaves() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 1920.0, 0.0), draft("eDP-1", 3840.0, 0.0)];
        drafts[1].scale = 2.0;
        settle(&mut drafts, 1, 16.0);
        assert!(contiguous(&drafts));
    }

    #[test]
    fn a_disabled_output_is_ignored() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 9000.0, 0.0), draft("eDP-1", 1920.0, 0.0)];
        drafts[1].enabled = false;
        settle(&mut drafts, 2, 16.0);
        assert_eq!((drafts[1].x, drafts[1].y), (9000.0, 0.0));
        assert!(contiguous(&drafts));
    }

    #[test]
    fn a_drop_near_an_edge_sticks_to_it() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 1928.0, 7.0)];
        settle(&mut drafts, 1, 16.0);
        assert_eq!((drafts[1].x, drafts[1].y), (1920.0, 0.0));
    }

    #[test]
    fn a_drop_under_two_outputs_sticks_to_their_centre() {
        let mut drafts = vec![draft("DP-1", 0.0, 0.0), draft("DP-2", 1920.0, 0.0), draft("eDP-1", 975.0, 1085.0)];
        settle(&mut drafts, 2, 32.0);
        assert_eq!((drafts[2].x, drafts[2].y), (960.0, 1080.0));
        assert!(contiguous(&drafts));
    }

    /// The wire reports a configured scale of 1.6 as 1.6015625; dividing by that
    /// gave a footprint 3px narrower than the compositor's own, so the next screen
    /// was placed 3px inside this one and showed a strip of it.
    #[test]
    fn a_footprint_is_the_compositor_s_own_so_nothing_overlaps() {
        let mut drafts = vec![draft("DP-2", 0.0, 0.0), draft("eDP-1", 2500.0, 0.0)];
        drafts[0].mode = Some(ModeSpec { width: 3840, height: 2160, refresh: Some(60_000) });
        drafts[0].scale = 1.6015625;
        assert_eq!(drafts[0].size(), egui::vec2(2400.0, 1350.0));
        settle(&mut drafts, 1, 16.0);
        assert_eq!(drafts[1].x, 2400.0);
        assert!(contiguous(&drafts));
    }

    #[test]
    fn the_guides_show_what_the_placement_lines_up_with() {
        let row = [rect_of(&draft("DP-1", 0.0, 0.0)), rect_of(&draft("DP-2", 1920.0, 0.0))];
        let centred = rect_of(&draft("eDP-1", 960.0, 1080.0));
        let lines = guides(centred, &row);
        // Centred on the pair: a line down the middle of the block.
        assert!(lines.contains(&[egui::pos2(1920.0, 0.0), egui::pos2(1920.0, 2160.0)]));

        let beside = rect_of(&draft("eDP-1", 3840.0, 0.0));
        let lines = guides(beside, &row);
        // Top edges flush with the output it sits next to.
        assert!(lines.iter().any(|&[a, b]| a.y == 0.0 && b.y == 0.0));
    }

    #[test]
    fn scale_and_rotation_change_the_footprint() {
        let mut draft = draft("DP-1", 0.0, 0.0);
        draft.scale = 2.0;
        assert_eq!(draft.size(), egui::vec2(960.0, 540.0));
        draft.transform = Transform::_90;
        assert_eq!(draft.size(), egui::vec2(540.0, 960.0));
    }
}

//! Reapply the matching profile when the set of connected outputs changes.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context as _, Result};
use calloop::{EventLoop, channel};
use calloop_wayland_source::WaylandSource;
use log::{error, info, warn};
use notify::{RecursiveMode, Watcher};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::config::Config;
use crate::matching::{best_match, fingerprint};
use crate::wl::{ApplyOutcome, HeadId, OutputState, Wayland};

#[derive(Debug, Clone, Copy)]
enum Wake {
    Reload,
    Quit,
}

pub fn run(path: PathBuf) -> Result<()> {
    let Wayland { conn, queue, mut state } = Wayland::connect()?;
    let qh = queue.handle();

    let mut event_loop: EventLoop<OutputState> = EventLoop::try_new()?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, queue).insert(handle.clone()).map_err(|e| e.error)?;

    let reload = Rc::new(Cell::new(false));
    let quit = Rc::new(Cell::new(false));

    // One channel for everything that is not a Wayland event.
    let (tx, rx) = channel::channel::<Wake>();

    // Watch the directory rather than the file: a rename-into-place (what
    // Config::save and most editors do) would break a watch on the file itself.
    // Everything else in that directory has to be filtered back out, or an
    // unrelated file would trigger a reload.
    let watched = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let watch_tx = tx.clone();
    let watched_file = path.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
        Ok(event) => {
            if event.paths.iter().any(|p| p == &watched_file) {
                let _ = watch_tx.send(Wake::Reload);
            }
        }
        Err(e) => error!("config watch failed: {e}"),
    })?;
    if let Err(e) = watcher.watch(&watched, RecursiveMode::NonRecursive) {
        warn!("not watching {}: {e}", watched.display());
    }

    // calloop 0.14's own signal source needs an unpublished nix, so signals go
    // through the same channel from a dedicated thread.
    let mut signals = signal_hook::iterator::Signals::new([SIGHUP, SIGINT, SIGTERM])?;
    std::thread::Builder::new().name("signals".into()).spawn(move || {
        for signal in signals.forever() {
            let wake = if signal == SIGHUP { Wake::Reload } else { Wake::Quit };
            if tx.send(wake).is_err() {
                break;
            }
        }
    })?;

    handle
        .insert_source(rx, {
            let reload = reload.clone();
            let quit = quit.clone();
            move |event, _, _| match event {
                channel::Event::Msg(Wake::Reload) => reload.set(true),
                channel::Event::Msg(Wake::Quit) => quit.set(true),
                channel::Event::Closed => {}
            }
        })
        .map_err(|e| e.error)?;

    let mut text = Config::read(&path)?;
    let mut config = Config::parse(&text).with_context(|| format!("in {}", path.display()))?;
    info!("loaded {} profile(s) from {}", config.profiles.len(), path.display());

    let mut applied: Option<Vec<HeadId>> = None;
    let mut generation = state.generation;
    let mut evaluate = true;

    while !quit.get() {
        // A watch event only means the file was touched: reload when its contents
        // really changed, so a rewrite with identical contents costs nothing.
        if reload.replace(false) {
            match Config::read(&path) {
                Ok(new_text) if new_text == text => {}
                Ok(new_text) => match Config::parse(&new_text) {
                    Ok(new) => {
                        info!("reloaded {} profile(s)", new.profiles.len());
                        text = new_text;
                        config = new;
                        // The config may now describe the current outputs differently.
                        applied = None;
                        evaluate = true;
                    }
                    Err(e) => error!("keeping the previous config: {e:#}"),
                },
                Err(e) => error!("keeping the previous config: {e:#}"),
            }
        }

        if state.generation != generation {
            generation = state.generation;
            evaluate = true;
        }

        if let Some(outcome) = state.take_outcome() {
            match outcome {
                ApplyOutcome::Succeeded => info!("configuration applied"),
                ApplyOutcome::Failed => warn!("compositor rejected the configuration"),
                ApplyOutcome::Cancelled => {
                    applied = None;
                    evaluate = true;
                }
            }
        }

        if evaluate {
            evaluate = false;
            apply_matching(&mut state, &qh, &config, &mut applied);
        }

        event_loop.dispatch(None, &mut state).context("event loop failed")?;
    }

    info!("exiting");
    Ok(())
}

/// Apply the best-matching profile, but only when the set of connected outputs
/// differs from the last one acted on. Layout changes that leave the set intact —
/// the arranger applying a drag, or a manual `swaymsg output` — are left alone.
fn apply_matching(
    state: &mut OutputState,
    qh: &wayland_client::QueueHandle<OutputState>,
    config: &Config,
    applied: &mut Option<Vec<HeadId>>,
) {
    let snapshot = state.snapshot();
    if snapshot.heads.is_empty() {
        return;
    }
    let current = fingerprint(&snapshot.heads);
    if applied.as_ref() == Some(&current) {
        return;
    }

    match best_match(config, &snapshot.heads) {
        Some(m) => {
            let name = m.profile.name.clone();
            match m.settings(&snapshot.heads) {
                Ok(settings) => {
                    info!("outputs changed, applying profile {name}");
                    if let Err(e) = state.configure(qh, &settings) {
                        error!("cannot apply profile {name}: {e:#}");
                        return;
                    }
                }
                Err(e) => {
                    error!("profile {name} is invalid: {e:#}");
                    return;
                }
            }
        }
        None => {
            let outputs: Vec<String> = snapshot.heads.iter().map(|h| h.id.to_string()).collect();
            warn!("no profile matches the connected outputs: {}", outputs.join(", "));
            warn!("run `wano save <name>` to remember the current arrangement");
        }
    }
    // Either way, don't reconsider this output set until it changes again.
    *applied = Some(current);
}

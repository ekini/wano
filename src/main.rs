use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};

use wano::config::{Config, profile_from_snapshot, suggested_name};
use wano::matching::{best_match, match_profile};
use wano::wl::{Wayland, transform_name};

#[derive(Parser)]
#[command(version, about = "Monitor layout profiles for wlroots compositors")]
struct Cli {
    /// Config file to read and write.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Watch for output changes and apply the matching profile.
    Daemon,
    /// Show the connected outputs and their modes.
    List,
    /// Remember the current arrangement as a profile.
    Save {
        /// Profile name. Defaults to a name derived from the connected monitors.
        name: Option<String>,
        /// Match monitors by model only, so the profile also fits an identical
        /// monitor with a different serial number.
        #[arg(long)]
        no_serial: bool,
    },
    /// Apply a saved profile now.
    Apply { name: String },
    /// Show which profile matches the connected outputs.
    Status,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).format_timestamp(None).init();

    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(Config::path);

    match cli.command {
        Command::Daemon => wano::daemon::run(path),
        Command::List => list(),
        Command::Save { name, no_serial } => save(&path, name, !no_serial),
        Command::Apply { name } => apply(&path, &name),
        Command::Status => status(&path),
    }
}

fn list() -> Result<()> {
    let wayland = Wayland::connect()?;
    for head in &wayland.snapshot().heads {
        let (lw, lh) = head.logical_size();
        println!("{}  {} {} {}", head.id.connector, head.id.make, head.id.model, head.id.serial);
        if head.description != head.id.connector && !head.description.is_empty() {
            println!("      {}", head.description);
        }
        print!("      {}", if head.enabled { "enabled" } else { "disabled" });
        if let Some(mode) = head.current_mode {
            print!("  {mode}");
        }
        println!(
            "  pos {},{}  scale {}  transform {}  logical {lw}x{lh}",
            head.position.0,
            head.position.1,
            head.scale,
            transform_name(head.transform),
        );
        let modes: Vec<String> =
            head.modes.iter().map(|m| format!("{m}{}", if m.preferred { "*" } else { "" })).collect();
        if !modes.is_empty() {
            println!("      modes: {}", modes.join("  "));
        }
    }
    Ok(())
}

fn save(path: &std::path::Path, name: Option<String>, with_serial: bool) -> Result<()> {
    let wayland = Wayland::connect()?;
    let snapshot = wayland.snapshot();
    if snapshot.heads.is_empty() {
        bail!("no outputs connected");
    }
    let name = name.unwrap_or_else(|| suggested_name(&snapshot));

    let mut config = Config::load(path)?;
    let replaced = config.profile(&name).is_some();
    config.upsert(profile_from_snapshot(&name, &snapshot, with_serial));
    config.save(path)?;

    println!("{} profile {name} in {}", if replaced { "replaced" } else { "saved" }, path.display());
    for output in &config.profile(&name).expect("just saved").outputs {
        println!("  {}", output.label());
    }
    Ok(())
}

fn apply(path: &std::path::Path, name: &str) -> Result<()> {
    let config = Config::load(path)?;
    let profile = config.profile(name).with_context(|| format!("no profile named {name:?}"))?;

    let mut wayland = Wayland::connect()?;
    let snapshot = wayland.snapshot();
    let m = match_profile(profile, &snapshot.heads)
        .with_context(|| format!("profile {name:?} does not describe the connected outputs"))?;
    wayland.apply(&m.settings(&snapshot.heads)?)?;

    println!("applied profile {name}");
    Ok(())
}

fn status(path: &std::path::Path) -> Result<()> {
    let config = Config::load(path)?;
    let wayland = Wayland::connect()?;
    let snapshot = wayland.snapshot();

    match best_match(&config, &snapshot.heads) {
        Some(m) => {
            println!("profile {} (score {})", m.profile.name, m.score);
            for (output, &head) in m.profile.outputs.iter().zip(&m.assignment) {
                println!("  {} -> {}", output.label(), snapshot.heads[head].id);
            }
        }
        None => {
            println!("no profile matches the connected outputs:");
            for head in &snapshot.heads {
                println!("  {}", head.id);
            }
            println!("run `wano save <name>` to remember the current arrangement");
        }
    }
    Ok(())
}

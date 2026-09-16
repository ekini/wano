# wano

Monitor layout manager for wlroots compositors (sway, river, …). One file holds
the whole arrangement — position, scale, mode, transform — a daemon reapplies it
when monitors come and go, and a GUI lets you drag the monitors instead of
computing coordinates by hand.

```
wano list              # connected outputs, with their modes
wano save [name]       # remember the current arrangement as a profile
wano apply <name>      # switch to a profile now
wano status            # which profile matches, and how it was assigned
wano daemon            # reapply the matching profile on hotplug
wano-ui                # arrange the monitors visually
```

Profiles live in `~/.config/wano/config.toml` (or `$XDG_CONFIG_HOME/wano`).

## Build

```
cargo install --path .
```

## Profiles

```toml
[[profile]]
name = "home"

[[profile.output]]
make = "Dell Inc."
model = "DELL P2723QE"
serial = "7HD1XY3"      # omit to match any monitor of this model
enabled = true
position = [0, 0]
scale = 1.6
mode = "3840x2160@59.997"
transform = "normal"

[[profile.output]]
connector = "eDP-1"     # internal panels match by connector: their serial is unreliable
position = [2400, 1350]
scale = 2.0
```

Each output needs at least one match key (`connector`, `make`, `model`,
`serial`); the rest are optional, and an absent field is left as the compositor
has it. `wano save` writes all of them, so the usual way to get a profile is to
arrange the screens and save.

### Matching

A profile applies only if its entries and the connected outputs pair up exactly:
every entry matched, every connected output used. A two-monitor profile
therefore does not apply when three are plugged in.

Among the profiles that fit, the most specific wins: an entry pinning a serial
or a connector counts double, one naming only make/model counts single, ties go
to file order. So one profile with serials covers a specific desk, and the same
profile without serials covers every other desk with those monitor models —
which is what replaces a `hnz_other1`…`hnz_other8` family of kanshi profiles.

## Daemon

`wano daemon` applies the matching profile when the *set* of connected outputs
changes. It deliberately ignores layout changes that leave that set alone, so it
never fights a manual `swaymsg output` or a drag in `wano-ui`. It also watches
the config file, so saving from the GUI takes effect immediately, and reloads on
`SIGHUP`.

Start it from the sway config:

```
exec wano daemon
```

## Arranger

`wano-ui` opens its own connection to the compositor and does not need the
daemon. Drag a monitor anywhere; when you drop it, it magnets against its
neighbours, leaving the layout gapless and free of overlap — a gap is dead
space the pointer cannot cross. A drop that lands near an alignment takes it:
edges flush, centres in line, or centred on the whole block the other monitors
form — so one screen below two lands exactly in the middle. While you drag, an
amber outline shows where the drop will land and amber guides show what it will
line up with; on release the monitor slides into place rather than jumping.
Changing scale, mode or transform re-settles the layout the same way.

Footprints are the compositor's own logical sizes — scale snapped to 120ths, the
division truncated — because a footprint even three pixels out is a strip of one
screen showing up on the other. The side panel sets
enable/disable, scale, mode and transform for the selected output. `Apply`
changes the live layout, `Revert` puts back the layout from when the GUI
started, `Save profile` writes the profile.

## Migrating from kanshi + sway `output` directives

wano owns position, scale, mode and transform. Sway reimposes its own `output`
directives on top of wlr-output-management, so any property still set in the
sway config will silently override wano.

1. Stop kanshi: remove the `exec … kanshi` and `exec_always … kanshi reload`
   lines from `~/.config/sway/config` (two daemons applying layouts will fight).
2. Delete the `output … pos/scale/mode/transform` lines from
   `~/.config/sway/config.d/local.conf`.
3. Keep `output … scale_filter` and `output … background` in the sway config:
   they are sway-specific and have no wlr-output-management equivalent.
4. Arrange the screens once per setup — `wano-ui`, then `Save profile` — or
   `wano save <name>` if the current layout is already right.

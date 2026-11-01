# OpenDeck Power Profile

An [OpenDeck](https://github.com/nekename/OpenDeck) plugin with one action, **Power
Profile**, assignable to a Stream Deck dial. It steps through the system's three power
profiles - **Power Saver**, **Balanced**, **Performance** - and always shows the active
one on the dial's touch strip.

It talks to whatever provides the `PowerProfiles` D-Bus interface on the system bus:
[power-profiles-daemon](https://gitlab.freedesktop.org/upower/power-profiles-daemon) or
[tuned-ppd](https://github.com/redhat-performance/tuned) (Fedora's default, which maps the
three profiles onto TuneD profiles). It uses the `org.freedesktop.UPower.PowerProfiles` bus
name, which tuned-ppd and power-profiles-daemon 0.20 and later export.

## Using the dial

- **Rotate** to step one profile at a time (Power Saver -> Balanced -> Performance),
  clamped at both ends - it won't wrap around. Each detent is one step, even when you turn
  faster than the provider confirms the previous one.
- **Press** to jump straight to Balanced, regardless of the current profile.
- Profiles the provider doesn't offer are skipped by clamping: power-profiles-daemon leaves
  Performance out on hardware without a platform-profile driver, and then the dial stops at
  Balanced (its bolt hint dims) instead of asking for a profile the provider would refuse.
  The plugin reads the provider's profile list once per connection.
- The touch strip follows Elgato's dial style: the profile name on top, a gauge in the
  middle whose needle points at the active profile, and leaf / bolt hints either side
  showing which way to turn. A hint dims when turning that way would do nothing.
- A line under the gauge says who holds the profile when an application has asked the
  provider to keep one (`Held by <app>`, e.g. GameMode asking for Performance; `+N` when
  several do), or, with no hold, why performance is limited (`Limited: lap detected` /
  `Limited: too hot`). Turning the dial during a hold still steps as usual; the provider
  then releases the hold and the line clears.
- The touch strip updates live even when the profile changes from somewhere else (a
  desktop applet, another key, `powerprofilesctl` in a terminal) - it subscribes to the
  provider's D-Bus change notifications rather than polling.
- **Cooling follows the profile** when [OpenLinkHub](https://github.com/jurkovic-nikola/OpenLinkHub)
  runs on this machine (its API on `127.0.0.1:27003`): after the provider confirms a change
  from one profile to another, from the dial or from anywhere else, fan channels whose
  speed profile is `Quiet`, `Normal` or `Performance` switch to the matching one (Power
  Saver -> `Quiet`, Balanced -> `Normal`, Performance -> `Performance`). Channels on any
  other curve, and pumps, are left alone; nothing is written when the plugin starts or
  reconnects.
  It is best effort: without OpenLinkHub, or if it doesn't answer within 2 seconds, the
  dial works as before and the plugin log gets one warning. Each switch is logged as
  `cooling sync: ...`.
- If the provider goes away (its process exits or its service is stopped) the dial shows
  "Unavailable" as soon as the bus reports its name gone, and the plugin tries to reconnect
  at once, then every 5 seconds; once the provider is back the dial shows the real current
  profile within about 5 seconds (at once after a quick restart). Both providers are D-Bus activatable, so on most systems a reconnect
  attempt starts the provider again by itself and "Unavailable" lasts only a moment, unless
  its service is masked.

## Installing

Download the latest `.streamDeckPlugin` from
[Releases](https://github.com/jfms7s/opendeck-power-profile/releases) (each release also
has a `SHA256SUMS` file to check it against: `sha256sum --check SHA256SUMS`), then either
double-click it (if your file manager associates the extension with OpenDeck) or unzip it
into `~/.config/opendeck/plugins/` and restart OpenDeck (plugins are only loaded at
startup).

## Manual smoke-test checklist

The D-Bus behaviour (connect, live updates, provider loss and restart, quick detents) is
covered by automated tests against a fake provider on a private bus. What still needs a
person is the real OpenDeck + Stream Deck + provider. Run this before publishing a release
(`<unit>` is `tuned-ppd` or `power-profiles-daemon`, whichever `systemctl status` finds),
and record the result per item: pass, fail or **not run** (with why). Each release's draft
notes start with this list, unticked: tick or annotate each item there before publishing.
The full run (date, OpenDeck version, provider and its version, Stream Deck model,
platform, per-item notes) goes in that release's smoke log, `releases/vX.Y.Z-smoke.md` in
the maintainer's project notes (kept outside this repository), which the release notes
link.

<!-- smoke-checklist:start (scripts/release-body.mjs copies everything up to the end marker into each draft release) -->

- [ ] Adding a Power Profile dial shows the current profile's name and a gauge whose
      needle points at it shortly after appearing.
- [ ] The leaf (left) icon dims at Power Saver and the bolt (right) icon dims at
      Performance; both dim while the dial shows "Unavailable".
- [ ] Rotating clockwise from Power Saver moves to Balanced, then Performance; rotating
      further clockwise stays at Performance (no wraparound).
- [ ] Rotating counter-clockwise from Performance moves to Balanced, then Power Saver;
      rotating further counter-clockwise stays at Power Saver (no wraparound).
- [ ] Two quick clockwise detents from Power Saver land on Performance.
- [ ] Pressing the dial from any profile jumps straight to Balanced.
- [ ] Changing the profile from outside updates the dial's touch strip immediately,
      without touching the dial: use the desktop's power applet, `powerprofilesctl set
      performance`, or (with no `powerprofilesctl`, as with tuned-ppd)
      `busctl set-property org.freedesktop.UPower.PowerProfiles
      /org/freedesktop/UPower/PowerProfiles org.freedesktop.UPower.PowerProfiles
      ActiveProfile s performance`.
- [ ] A hand-made hold shows on the strip and clears. Providers drop a hold when its
      caller leaves the bus, so it needs a process that stays connected (`busctl call`
      exits at once); with the dial at Balanced, run
      `python3 -c 'from gi.repository import Gio, GLib; b = Gio.bus_get_sync(Gio.BusType.SYSTEM); b.call_sync("org.freedesktop.UPower.PowerProfiles", "/org/freedesktop/UPower/PowerProfiles", "org.freedesktop.UPower.PowerProfiles", "HoldProfile", GLib.Variant("(sss)", ("performance", "smoke test", "smoke-app")), None, 0, -1, None); GLib.MainLoop().run()'`
      (needs PyGObject, which tuned-ppd already pulls in). The dial moves to Performance
      and shows "Held by smoke-app"; Ctrl+C releases the hold and the line clears. Run it
      again and turn the dial instead: it steps as usual and the line clears.
- [ ] With OpenLinkHub running and a fan channel on `Quiet`, `Normal` or `Performance`:
      turning the dial to Performance switches that channel to `Performance` within a
      second (OpenLinkHub's page shows it, and the plugin log has a `cooling sync:` line);
      turning back to Balanced switches it to `Normal`. A channel on a custom curve stays
      as it was. Restarting OpenDeck does not change any channel.
- [ ] `sudo systemctl mask --runtime <unit> && sudo systemctl stop <unit>` makes the dial
      show "Unavailable" within a second or two and keeps it there (masking stops the
      plugin's reconnect from D-Bus-activating the provider again). Over the next minute
      the plugin log gains two `[WARN]` lines (provider lost, then not reachable) and no
      `[INFO]` lines from `zbus` (its connection handshake).
- [ ] `sudo systemctl unmask --runtime <unit> && sudo systemctl start <unit>` makes the
      dial recover within about 5 seconds and show the real current profile.
- [ ] The plugin log (`~/.local/share/opendeck/logs/plugins/com.jfms7s.powerprofile.sdPlugin.log`)
      has no `[ERROR]` lines from the session.

<!-- smoke-checklist:end -->

## Development

The tests start their own private `dbus-daemon` (package `dbus-daemon` or `dbus`), so they
need that binary installed but never touch the system bus or the active profile. These are
the commands CI runs:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
node build.mjs                               # assembles dist/<uuid>.sdPlugin for this host
cp -r dist/com.jfms7s.powerprofile.sdPlugin ~/.config/opendeck/plugins/
# restart OpenDeck, then work through the smoke-test checklist above
```

`node build.mjs <triple>...` packages specific `--target` builds instead; add `--release`
to require every architecture in the manifest's `CodePaths` and also write
`dist/<bin>.streamDeckPlugin` and `dist/SHA256SUMS`.

## Releasing

1. Bump the version in `Cargo.toml` and `assets/manifest.json` together (a unit test fails
   if they differ) and merge that to `master`.
2. Push a tag `v<version>`. The Release workflow re-runs the checks, builds x86_64 and
   aarch64, refuses a tag that doesn't match the version, and creates a **draft** release
   with the bundle and `SHA256SUMS` attached. The draft's notes start with the smoke-test
   checklist above, unticked (`scripts/release-body.mjs`), followed by the generated
   change list.
3. Install the drafted bundle, run the smoke-test checklist, write the release's smoke
   log, tick or annotate every item in the draft's notes and link the log, then publish
   the draft. A draft whose checklist is still unticked has not been smoke-tested. Ansible installs the latest *published* release, so publishing is what deploys it.
   Releases are immutable once published (GitHub's immutable releases setting, on for
   this repository): the assets and the tag are frozen, so a broken release is fixed with
   a new version, never by replacing an asset. Before publishing, check that
   `gh api repos/jfms7s/opendeck-power-profile/immutable-releases` still reports
   `"enabled":true`; a release published while it is off stays mutable.

## License

MIT — see [LICENSE](LICENSE).

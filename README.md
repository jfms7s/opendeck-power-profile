# OpenDeck Power Profile

An [OpenDeck](https://github.com/nekename/OpenDeck) plugin with one action, **Power
Profile**, assignable to a Stream Deck dial. It steps through the system's three power
profiles - **Power Saver**, **Balanced**, **Performance** - and always shows the active
one on the dial's touch strip.

It talks to whatever provides the `PowerProfiles` D-Bus interface on the system bus:
[power-profiles-daemon](https://gitlab.freedesktop.org/upower/power-profiles-daemon) or
[tuned-ppd](https://github.com/redhat-performance/tuned) (Fedora's default, which maps the
three profiles onto TuneD profiles). It uses the `org.freedesktop.UPower.PowerProfiles` bus
name and falls back to the legacy `net.hadess.PowerProfiles` name for older
power-profiles-daemon releases that only export that one.

## Using the dial

- **Rotate** to step one profile at a time (Power Saver -> Balanced -> Performance),
  clamped at both ends - it won't wrap around. Each detent is one step, even when you turn
  faster than the provider confirms the previous one.
- **Press** to jump straight to Balanced, regardless of the current profile.
- The touch strip follows Elgato's dial style: the profile name on top, a gauge in the
  middle whose needle points at the active profile, and leaf / bolt hints either side
  showing which way to turn. A hint dims when turning that way would do nothing.
- The touch strip updates live even when the profile changes from somewhere else (a
  desktop applet, another key, `powerprofilesctl` in a terminal) - it subscribes to the
  provider's D-Bus change notifications rather than polling.
- If the provider goes away (its process exits or its service is stopped) the dial shows
  "Unavailable" as soon as the bus reports its name gone, and the plugin tries to reconnect
  every 5 seconds; once the provider is back the dial shows the real current profile within
  about 5 seconds. Both providers are D-Bus activatable, so on most systems a reconnect
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
(`<unit>` is `tuned-ppd` or `power-profiles-daemon`, whichever `systemctl status` finds):

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
- [ ] `sudo systemctl mask --runtime <unit> && sudo systemctl stop <unit>` makes the dial
      show "Unavailable" within a second or two and keeps it there (masking stops the
      plugin's reconnect from D-Bus-activating the provider again).
- [ ] `sudo systemctl unmask --runtime <unit> && sudo systemctl start <unit>` makes the
      dial recover within about 5 seconds and show the real current profile.
- [ ] The plugin log (`~/.local/share/opendeck/logs/plugins/com.jfms7s.powerprofile.sdPlugin.log`)
      has no `[ERROR]` lines from the session.

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
   with the bundle and `SHA256SUMS` attached.
3. Install the drafted bundle, run the smoke-test checklist, then publish the draft.
   Ansible installs the latest *published* release, so publishing is what deploys it.

## License

MIT — see [LICENSE](LICENSE).

# OpenDeck Power Profile

An [OpenDeck](https://github.com/nekename/OpenDeck) plugin with one action, **Power
Profile**, assignable to a Stream Deck dial. It steps through
[power-profiles-daemon](https://gitlab.freedesktop.org/upower/power-profiles-daemon)'s
three profiles - **Power Saver**, **Balanced**, **Performance** - and always shows the
active one on the dial's touch strip.

## Using the dial

- **Rotate** to step one profile at a time (Power Saver -> Balanced -> Performance),
  clamped at both ends - it won't wrap around.
- **Press** to jump straight to Balanced, regardless of the current profile.
- The touch strip follows Elgato's dial style: the profile name on top, a gauge in the
  middle whose needle points at the active profile, and leaf / bolt hints either side
  showing which way to turn. A hint dims when the dial can't go any further that way.
- The touch strip updates live even when the profile changes from somewhere else (a GUI
  applet, another key, `powerprofilesctl` in a terminal) - it subscribes to
  power-profiles-daemon's D-Bus change notifications rather than polling.
- If power-profiles-daemon isn't running or the system bus is unreachable, the dial shows
  "Unavailable" and the plugin retries connecting every 5 seconds in the background.

## Installing

Download the latest `.streamDeckPlugin` from
[Releases](https://github.com/jfms7s/opendeck-power-profile/releases), then either
double-click it (if your file manager associates the extension with OpenDeck) or unzip it
into `~/.config/opendeck/plugins/` and restart OpenDeck (plugins are only loaded at
startup).

## Manual smoke-test checklist

Run this against a live OpenDeck + Stream Deck session with power-profiles-daemon running,
before cutting a release:

- [ ] Adding a Power Profile dial shows the current profile's name and a gauge whose
      needle points at it shortly after appearing.
- [ ] The leaf (left) icon dims at Power Saver and the bolt (right) icon dims at
      Performance; both dim while the dial shows "Unavailable".
- [ ] Rotating clockwise from Power Saver moves to Balanced, then Performance; rotating
      further clockwise stays at Performance (no wraparound).
- [ ] Rotating counter-clockwise from Performance moves to Balanced, then Power Saver;
      rotating further counter-clockwise stays at Power Saver (no wraparound).
- [ ] Pressing the dial from any profile jumps straight to Balanced.
- [ ] Running `powerprofilesctl set performance` in a terminal updates the dial's touch
      strip immediately, without touching the dial.
- [ ] Stopping power-profiles-daemon (`systemctl stop power-profiles-daemon`) makes the
      dial show "Unavailable"; starting it again
      (`systemctl start power-profiles-daemon`) makes it recover within ~5 seconds and
      show the real current profile.

## Development

```bash
cargo test                                   # unit tests (no live D-Bus needed)
cargo build --release --target <triple>
node build.mjs <triple>                      # assembles dist/<uuid>.sdPlugin
cp -r dist/com.jfms7s.powerprofile.sdPlugin ~/.config/opendeck/plugins/
# restart OpenDeck, then work through the smoke-test checklist above
```

## License

MIT — see [LICENSE](LICENSE).

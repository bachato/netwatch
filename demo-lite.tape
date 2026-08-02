# NetWatch Lite — the one-screen view.
#
# Story: ten tabs is an operator's instrument. When the question is just
# "what's using my network, and is my connection OK?", `--lite` answers it on
# one 80x24 screen with six keys — and `L` escalates to the full TUI without
# restarting anything, because both views share the same collectors.
#
# Prereqs learned the hard way:
#   - Traffic must start AFTER netwatch is capturing, or the HOST column shows
#     raw IPs: SNI is read from the ClientHello, so a handshake that predates
#     capture is unreadable. Hence the `sleep 6` before the generator.
#   - The generator uses long-lived rate-limited downloads to DISTINCT hosts.
#     Short bursts finish between connection snapshots and the rows flicker.
#   - Terminal must be at least 80x24 or Lite renders its too-small notice
#     instead of the layout. Width/Height below are sized for exactly that.
#
# Record with:  vhs demo-lite.tape
# Needs: ./target/release/netwatch, vhs, scripts/demo-lite-traffic.sh.

Output demo-lite.gif

Set Shell "bash"
Set FontSize 16
Set Width 860
Set Height 500
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms
Set Framerate 18
Set Padding 10

# Hidden setup: arm the traffic generator to start a few seconds from now, so
# its handshakes land after netwatch is already capturing and the HOST column
# resolves to real names instead of IPs.
#   - `netwatch` on PATH may be an older installed copy. The shim below points
#     it at this repo's release build for the recording only, so the GIF shows
#     the command users actually type without touching ~/.cargo/bin.
#   - HOME is redirected to a scratch tree pinning `theme = "terminal"`, so the
#     colours come from the terminal palette rather than netwatch's own. Without
#     this the GIF inherits whatever theme the operator happens to have set.
Hide
Type "export PATH=$PWD/target/release:$PATH"
Enter
Type "export HOME=$(./scripts/demo-lite-env.sh)"
Enter
Type "(sleep 6; ./scripts/demo-lite-traffic.sh >/dev/null 2>&1) &"
Enter
Type "clear"
Enter
Show

# Launch. No sudo, no flags beyond --lite, no config.
Type "netwatch --lite"
Enter

# Let the charts start filling and the first named transfers appear. The
# throughput window is one sample per column at 1 Hz, so it fills from the
# right in real time — no fast-forward.
Sleep 18s

# Move to the second live transfer. Selection is stable — rows are ordered by
# rate with a total tiebreak, so nothing reshuffles under the cursor. One step
# only: the top two rows are the active downloads, and landing on an idle row
# would expand a detail block with nothing in it.
Type "j"
Sleep 2s

# Expand the selected talker in place: remote endpoint, protocol, state,
# connection count, rates and handshake RTT. This is the whole Connections
# tab, folded into three lines under the row it belongs to.
Enter
Sleep 5s

# Collapse.
Escape
Sleep 1500ms

# Filter incrementally by process or host.
Type "/"
Sleep 800ms
Type "curl"
Sleep 3s
Enter
Sleep 2500ms

# Clear the filter.
Escape
Sleep 2500ms

# Deliberately NOT shown: `L` escalating to the full ten-tab TUI. It works, but
# the full view wants ~130 columns and renders clipped in this 80-column frame —
# an honest picture that reads as a broken one. That story belongs in prose.

Type "q"
Sleep 1s

# Tear down the traffic generator.
Hide
Type "pkill -f demo-lite-traffic.sh; pkill -f 'curl -s --limit-rate'"
Enter
Sleep 500ms

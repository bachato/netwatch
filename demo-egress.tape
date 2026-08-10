# NetWatch — egress policy: observe → promote → warn.
#
# Story: netwatch watches what each program talks to, one keypress promotes
# that observed baseline into a policy, and from then on it says something
# when that program starts talking somewhere new. Observe-only; never blocks.
#
# Prereqs learned the hard way:
#
#   - Destination NAMES come from the TLS ClientHello, so traffic must start
#     AFTER netwatch is capturing. A handshake that predates capture leaves a
#     row showing a raw IP forever, which makes the feature look worse than
#     it is. Hence the delayed generator below.
#
#   - Capture needs BPF access. On macOS that is Wireshark's ChmodBPF group
#     (`access_bpf`); without it, launch with sudo and the same tape works.
#
#   - HOME is redirected to a scratch tree so the recording never touches the
#     operator's real egress-policy.toml, and — more importantly — so every
#     take starts with NO policy. A demo about promoting a baseline is a lie
#     if the baseline was already promoted in the previous take.
#
#   - **Both traffic phases must be armed before netwatch launches.** There is
#     no way to run a command mid-recording: netwatch owns the terminal, so a
#     `Hide`/`Type`/`Show` block sends its characters to the TUI as keystrokes,
#     not to a shell. The first take of this tape did exactly that, and the
#     `/` in a script path opened the egress filter and typed a path into it.
#     Both phases are therefore `sleep N &` jobs on a timeline that the Sleep
#     directives below have to stay in step with.
#
#   - The promote must land on `curl`, and the volume sort puts a browser on
#     top. Filtering to curl first is what makes the promoted rule belong to
#     the program that later drifts — and it shows the filter off on the way.
#
# Timeline (recording seconds, measured from netwatch launch):
#   t+5   baseline traffic starts     — curl reaches two named hosts
#   t+20  filter to curl, unfold      — what was learned
#   t+27  promote                     — rule written to egress-policy.toml
#   t+40  drift traffic starts        — same program, somewhere new
#   t+55  sort by risk                — the finding floats to the top
#
# Record with:  vhs demo-egress.tape
# Needs: ./target/release/netwatch, vhs, scripts/demo-egress-traffic.sh.

Output demo-egress.gif

Set Shell "bash"
Set FontSize 16
# ~130 columns. The full ten-tab view clips its tab bar and footer below that,
# which reads as a broken render rather than an honest one.
Set Width 1440
Set Height 760
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms
Set Framerate 18
Set Padding 10

Hide
Type "export PATH=$PWD/target/release:$PATH"
Enter
Type "export HOME=$(./scripts/demo-lite-env.sh)"
Enter
# Both phases armed now, on the timeline in the header comment.
Type "(sleep 5; ./scripts/demo-egress-traffic.sh baseline >/dev/null 2>&1) &"
Enter
Type "(sleep 40; ./scripts/demo-egress-traffic.sh drift >/dev/null 2>&1) &"
Enter
Type "clear"
Enter
Show

# Launch and go straight to the Egress tab.
Type "netwatch"
Enter
Sleep 5s

Type "0"
Sleep 13s

# Every process reads "— no rule": there is no policy yet, so nothing has been
# checked. The tab says that rather than implying everything is fine.

# Filter to the program we're going to declare.
Type "/"
Sleep 800ms
Type "curl"
Sleep 2s
Enter
Sleep 2s

# Unfold it: the destinations it has actually reached, with the port and the
# autonomous system behind each.
Type " "
Sleep 5s

# Promote — the observed baseline becomes a rule in egress-policy.toml.
Enter
Sleep 5s

# Wait for the drift traffic to land: the same program reaching somewhere it
# has never been, now that a rule exists. The filter stays on curl —
# deliberately, so the story stays on the one program being declared.
Sleep 20s

# Sort by risk — the ordering that answers "what should I look at" rather
# than "what is biggest". The picker navigates with j/k; typing the mode name
# would hit `s`, which is also the toggle, and close it again.
Type "s"
Sleep 1500ms
Type "jjjj"
Sleep 1200ms
Enter
Sleep 7s

Type "q"
Sleep 1s

Hide
Type "pkill -f demo-egress-traffic.sh; pkill -f 'curl -s --limit-rate'"
Enter
Sleep 500ms

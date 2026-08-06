# Roadmap

What is not built yet, and why each thing is or is not next. Finished work moves
to [roadmap-done.md](roadmap-done.md) rather than being deleted — the reasoning
behind a decision outlives the decision.

Ordered by what stands between Adit and someone else being able to use it, not
by how interesting the work is.

---

## Blocked on someone else

- **OneDrive client id.** The code path is complete and identical to the other
  two drives; the app registration has not come back. When it does it is one
  command — `gh secret set ADIT_SYNC_ONEDRIVE_CLIENT_ID` — and no code change.
  Until then the panel reports the provider as unconfigured, by design.

## Needs a real device, not more code

- **Telnet against actual hardware.** The IAC negotiation is verified against
  the RFCs and a loopback harness, never against a Cisco switch or an IPMI
  console. The `CR NUL` translation is the specific thing to watch: it is
  spec-correct, and exactly the sort of detail real equipment disagrees about.

## Adoption

- **Code signing, Windows and macOS.** Currently the biggest thing between this
  and other people running it: SmartScreen blocks an unsigned installer behind
  an "unknown publisher" warning, and macOS requires a right-click to open the
  dmg. Mostly a procurement task — an OV/EV certificate and an Apple Developer
  ID — rather than an engineering one. Parked pending a decision to spend.

## Known gaps worth closing

- **The English translation is partial.** Menus, the settings page, the dialogs
  and the runtime notices are covered; a missing string falls back to the
  Chinese original rather than showing a key, so the gap is invisible until you
  meet it. Adding one is a row in `i18n.rs` and touches no call site.
- **RDP: the server cursor shape is not drawn.** The pointer is the local OS's,
  not the remote host's.
- **RDP dirty rects coalesce by count, not by cost.** Sixteen regions are kept
  and the cheapest pair merges past that. A better policy would weigh bytes
  saved against per-message overhead instead of using a fixed cap.
- **Terminal: no combining or zero-width characters, no DCS/Sixel, no charset
  designation, no custom tab stops.** `TerminalChangeSet` dirty-row tracking is
  a stub that always reports the whole screen.
- **MFA does not cover the dial fallback.** SFTP, tunnels and the SFTP protocol
  ride an existing SSH session where there is one, and answer no challenge.
  With nothing to ride on they dial their own connection, and that path is
  non-interactive — so an MFA host cannot be reached by SFTP once its shell has
  exited.
- **Jump hosts reuse the target's single credential.** No per-hop
  authentication.
- **SFTP shell has no line-at-a-time mode**, and its history is reachable with
  the arrow keys but not searchable.
- **Debian has no logo.** Deliberately: the swirl's identity is its taper and
  asymmetry, and the only version computable by hand is a constant-width arc
  spiral — a different mark that happens to be red. A diamond never claimed to
  be anything; a bad swirl would. Wants real vector art rather than more effort.

## Considered and not doing

- **VNC.** A full RFB client means the protocol, several authentication types,
  and Raw/CopyRect/Hextile/Tight/ZRLE encodings — comparable in size to the
  entire RDP subsystem. Not worth it without a machine that genuinely needs it.
- **Automatic sync on change.** The setting existed, did nothing, and was
  removed rather than wired up. Syncing on every change would have spread this
  project's one mass-deletion bug faster than a manual button did, and the guard
  that caught it was a human deciding when to press. Worth revisiting once the
  merge has more history behind it.
- **Guessing a host's OS from its name.** Wrong often enough to be worse than
  nothing. The protocol *is* evidence — RDP means Windows — and so is an SSH
  banner (`SSH-2.0-OpenSSH_8.9p1 Ubuntu-3ubuntu0.4`), so reading the banner
  after connect is the version of this idea worth building.

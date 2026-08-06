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
  dmg. Mostly a procurement task — a certificate and an Apple Developer ID —
  rather than an engineering one. Parked pending a decision to spend.

  The researched Windows route is **Azure Trusted Signing**: cloud-only, around
  $10/month, no USB hardware token, and built for CI — `azure/login` over OIDC
  federated credentials then `azure/trusted-signing-action`, signing both
  `adit-app.exe` and the installer with an RFC3161 timestamp so the signatures
  outlive the certificate. The reason it matters more than it looks: SmartScreen
  scores *publisher-certificate* reputation as well as per-file hashes, so an
  unsigned build starts from zero every single release and never accumulates
  anything. The prerequisite is an organisation identity on the Azure account;
  the CI work is small once that exists.

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
- **`ProxyCommand`, and `ProxyJump` on import.** Jump chains are typed into the
  profile by hand: the `~/.ssh/config` importer reads `Host` / `HostName` /
  `User` / `Port` / `IdentityFile` and drops everything else, so a `ProxyJump`
  line does not become a chain. `ProxyCommand` has no implementation at all,
  though the `connect_stream` transport the jump chain already uses would carry
  it — a spawned process's stdin/stdout joined as the stream. Token expansion
  (`%h`/`%p`/`%r`) and quoting on Windows is the part to be careful with; it is
  an injection surface.
- **No profile export, and no PuTTY session import.** Import covers
  `~/.ssh/config` and a SecureCRT session tree; nothing goes the other way, and
  PuTTY's registry sessions are unread (its `.ppk` *keys* work).
- **Per-profile session options stop at the startup command and terminal type.**
  No per-profile environment variables and no character-encoding override.
- **Logging is app-wide.** The folder, the filename pattern, auto-start and the
  plaintext mode are one global setting; there is no per-profile policy and no
  keystroke (input) log.
- **Nothing runs on a pattern.** Snippets ship, and keyword highlighting is the
  colouring half of what SecureCRT calls triggers, but no *action* fires when a
  pattern appears in the output.
- **Appearance is process-global.** `TERM_SCHEME` is a process-wide atomic, so
  font, size and colour scheme are one setting for every session at once. That
  blocks three things behind one refactor: per-profile scheme overrides,
  user-defined schemes, and the per-pane background tint that would extend the
  PROD badge from the tab into the terminal itself.
- **Bare URLs are not detected.** An OSC 8 link is clickable; a plain
  `https://…` the server printed without markup is not, and a linked run does
  not underline on hover.
- **A rejected host key is not remembered**, and there is no "connect once" that
  trusts a key for one session without writing it to `known_hosts`.
- **Reconnect is one global policy.** The backoff curve and the attempt cap are
  compiled in and the toggle is app-wide; no per-profile override.
- **A tunnel does not survive its own connection dropping.** Each tunnel runs its
  own SSH connection, and when that connection dies the tunnel stops rather than
  re-dialling.
- **SFTP transfers have no overwrite confirmation, no "download as", and no
  resume.**
- **Panes cannot be detached** into their own window and re-attached.
- **High-DPI and accessibility have never had a pass.**
- **Test gaps against the real `sshd`.** The Docker suite covers password accept
  and reject, key auth, encrypted keys with right and wrong passphrases, SFTP
  files and directory trees, local and remote forwarding, and jump hosts through
  a bastion. Untested: dynamic (SOCKS) forwarding, agent auth, and
  keyboard-interactive — the last one deliberately, because a real
  PAM/TOTP container is time-based and therefore flaky, so the interactive-MFA
  path has no end-to-end test at all.

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
- **Zmodem (`rz` / `sz`).** Researched, planned, and deliberately deferred. For
  Adit it is parity, not capability: the SFTP panel is strictly more capable —
  browsing, recursive directories, a queue — and what ZMODEM adds is *workflow*,
  typing `sz file` at whatever prompt you happen to be sitting at, including
  through sudo, a jump host, or a nested shell. Against that it is the highest
  risk item on this list, because it takes over the live shell channel in-band:
  a sentry on the output stream watches for the ZRQINIT trigger, then the
  channel stops being a terminal until the transfer ends. Revisit on explicit
  demand rather than on principle.
- **A plugin system.** Named out of scope for the first native milestone and
  never revisited since. Unlike the protocol non-goals beside it
  ([decisions.md #15](decisions.md#15-more-protocols-than-ssh--reversal)), this
  one still stands.

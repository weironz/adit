# Adit — Phase 2 Development Plan

Date: 2026-07-11
Status: Planned. Next-stage backlog after the v0.1.x feature push. Complements
[feature-roadmap.md](feature-roadmap.md) (the living Phase A–E backlog) — this
document is the researched, best-practice-backed plan for the next batch, with a
concrete approach, acceptance criteria, and effort/risk per item.

Each item was researched against real industry clients (OpenSSH, PuTTY,
SecureCRT/Xshell, Termius, iTerm2, WezTerm, Windows Terminal) and open-source
implementations before writing the plan. Sources are cited inline per item.

## Priority & order

Recommended build order (foundational first, then the capability gaps for the
SecureCRT/Xshell audience, then polish):

| # | Item | Effort | Risk | Why now |
|---|------|--------|------|---------|
| 1 | **Integration tests vs. real `sshd`** (Docker in CI) | L | med | Foundation — de-risks every change below; first real-server verification |
| 2 | **Host-key policy + known_hosts UI** | S–M | low | Closes an actual security hole (silent auto-trust of new keys) |
| 3 | **ProxyJump / jump host** (bastion chaining) | L | med | Biggest capability gap — corporate users can't reach internal hosts without it |
| 4 | **Interactive MFA** (keyboard-interactive multi-prompt) | M | med | Can't log into any 2FA/OTP/Duo server today |
| 5 | **Key passphrase + `.ppk` support** | L | med | Encrypted keys and PuTTY `.ppk` are common on Windows |
| 6 | **Per-session appearance** (color-code prod/staging) | M | med | Prevents "ran it on the wrong server" mistakes; cheap |
| 7 | **Windows code signing** (Authenticode) | M | low | SmartScreen wall on every release; blocks adoption |
| 8 | **OSC 8 hyperlinks + URL click** | L | med | Common terminal expectation |
| 9 | **Zmodem (rz/sz)** | L | high | Deferred — SFTP already covers the need; revisit on demand |

Legend: Effort S/M/L/XL, Risk low/med/high.

---

## 1. Integration tests against a real `sshd` (Docker in CI)

**Status: ✅ initial suite landed.** `crates/adit-ssh/tests/integration.rs`
(feature `integration`) drives the `spawn_*` API against a throwaway OpenSSH
container (`docker/test-sshd/`), with a new `ubuntu-latest` CI job. Covered:
password auth (accept + reject), SFTP single-file **and** recursive-directory
round-trips, and a local port-forward round-trip to the remote sshd. The default
`cargo test --workspace` (Windows, no Docker) skips it. Still open: dynamic/remote
forwarding, agent auth, and keyboard-interactive (blocked on the MFA work, item 4).

**What & why.** Everything shipped in the recent push (recursive SFTP, config
relocation, rename, etc.) was verified by review + unit tests — never against a
real server or the real GUI. In-process russh tests can't catch interop bugs with
the OpenSSH `sshd` users actually connect to (banner/kex/cipher negotiation, PAM,
`AllowTcpForwarding`/`GatewayPorts` semantics). This is the highest-value gap in
the test story. Drive it at the `adit-ssh`/`adit-session` layer (the `spawn_*`
handle + event-poll API), bypassing iced entirely.

**References.** russh's own suite is in-process client↔server (validates protocol,
not interop). `libssh2`/`ssh2-rs` and `wezterm-ssh` run a **real `sshd`** in CI via
setup scripts — and wezterm's harness famously hit pubkey-auth **hangs only in
GitHub Actions** (a readiness/timing bug), the canonical warning here. OSS:
[`testcontainers-rs`](https://github.com/testcontainers/testcontainers-rs) 0.27
(programmatic container lifecycle from the test binary, random host port, auto
teardown), [`linuxserver/openssh-server`](https://docs.linuxserver.io/images/docker-openssh-server)
(`PASSWORD_ACCESS`, `USER_PASSWORD`, `PUBLIC_KEY`; listens on 2222). RFCs to keep
the matrix honest: 4252 (auth), 4254 §7 (port forwarding), 4256 (keyboard-interactive).

**Approach for Adit.** New `crates/adit-ssh/tests/integration/` suite, feature-gated
(`[features] integration = []` or `#[ignore]`) so the default `cargo test --workspace`
(Windows CI) stays green without Docker. `testcontainers` 0.27 as a dev-dependency; a
small `docker/test-sshd/Dockerfile` baking an `sshd_config` that enables everything
the matrix needs (`PasswordAuthentication`, `PubkeyAuthentication`,
`KbdInteractiveAuthentication`, `AllowTcpForwarding`, `GatewayPorts clientspecified`,
`PermitTunnel`) + a committed **throwaway** test keypair + a test user. A
`common::TestServer` helper starts the container, waits on the sshd listening line,
returns `(host, port)` via `get_host_port_ipv4(2222)`. A shared
`pump_until(handle, predicate, timeout)` polls `try_recv()`. New CI job on
`ubuntu-latest`: `cargo test -p adit-ssh --features integration -- --include-ignored`.

**Best practices.** Test at the `spawn_*` boundary, not the GUI. Prefer
testcontainers over GHA `services:` (random port + real readiness + runs locally
too). Pin the image by **digest**, not `latest`. Seed deterministic key fixtures;
never reuse real keys. Assert the actual bytes/SFTP result (echo a nonce and match
it; upload→download and diff; open a forward and complete a round-trip), not just
"no error". Cover the negative path (wrong password ⇒ `AuthRejected`) and TOFU
(new key recorded; **changed** key still surfaces the prompt/error).

**Acceptance.** (a) password auth accept + reject; (b) pubkey auth via identity
file; (c) keyboard-interactive; (d) SFTP single-file **and** a directory tree,
byte/tree-asserted; (e) local + dynamic(SOCKS) + remote forwarding with a real
round-trip to a non-routable in-container service; (f) keepalive keeps an idle
session alive past sshd's interval; (g) the job is green on `ubuntu-latest` and the
default Windows `cargo test` needs no Docker.

---

## 2. Host-key policy + known_hosts management UI

**What & why.** Adit currently **auto-trusts a brand-new host key silently** (a
*changed* key still prompts). That is strictly weaker than OpenSSH `accept-new`
and defeats first-connect MITM protection — the entire point of Trust-On-First-Use.
No mainstream client does this; all prompt on first connect and show the fingerprint.

**References.** OpenSSH `StrictHostKeyChecking` modes `yes` / `accept-new` (added
7.6, 2017) / `ask` (the interactive default) / `no|off`
([ssh_config(5)](https://man.openbsd.org/ssh_config)). `known_hosts` format —
hashed hostnames, `@revoked` (hard-fail) and `@cert-authority` markers, multiple
key types per host, `[host]:port` notation ([sshd(8)](https://man.openbsd.org/sshd.8)).
First-connect UX: PuTTY (Accept / Connect Once / Cancel + the loud "POTENTIAL
SECURITY BREACH" changed-key dialog), SecureCRT (Accept & Save / Accept Once,
manageable from Global Options), Termius ("trust the fingerprint of this host?").

**Approach for Adit.** Move to **`accept-new`-with-a-prompt** as the interactive
default: on an unknown host, show a first-connect dialog with **SHA256 fingerprint
+ key type + host:port** and a three-way choice (**Trust & Save** / **Connect
Once** / **Cancel**, Cancel default). Keep — and strengthen — the changed-key
warning (old vs. new fingerprint side-by-side, name the MITM risk, default Cancel).
Keep the existing **"auto-accept new host keys"** as an explicit **opt-in** toggle
for batch/non-interactive workflows (never the global default). Add a **known_hosts
management screen**: list `(host:port, keytype) → SHA256 fingerprint`, delete an
entry (⇒ next connect prompts as first-use). Keep the on-disk file OpenSSH-format
compatible and make the **parser tolerant** of `[host]:port`, multiple key types,
hashed lines, and `@revoked`/`@cert-authority` (so imports don't silently drop
security markers), even before the writer emits them.

**Pitfalls.** Keying by host only (not `(host, keytype)`) mis-flags a valid second
key type as "changed", training users to click through real warnings. Ignoring the
port cross-trusts unrelated daemons. A one-click "update key" with no fingerprint
comparison lets users click past a real MITM. Show OpenSSH **SHA256** so users can
verify against `ssh-keygen`/provider consoles.

**Acceptance.** (a) never-seen host prompts with SHA256+type+host:port; key not
persisted unless "Trust & Save"; (b) "Connect Once" doesn't write known_hosts;
(c) changed key blocks by default, shows stored-vs-offered, proceeds only on
explicit update; (d) a host with two key types stores/matches both without a false
"changed"; (e) same host on two ports = two independent entries; (f) with the opt-in
toggle ON, a new host connects silently but a **changed** key still blocks; (g) the
UI lists and deletes entries; deletion reverts to first-connect.

---

## 3. ProxyJump / jump host (bastion chaining) + ProxyCommand

**Status: ✅ shipped in v0.1.31 (jump-host chaining).** `JumpHop` on the profile
(serde default; comma-separated `user@host:port` chain in the SSH editor, IPv6-aware
parsing with save-time validation), `connect_through_jumps` chains hops (hop 0 via
`client::connect`, each further hop via `direct-tcpip` + `connect_stream`), and
shell / SFTP / tunnel all connect through the same chain with the intermediate
`Handle`s kept alive. Each hop is a real handshake so every host key — bastions and
target — is verified through the tunnel; the shell path keeps the interactive
first-use prompt for the **target** key. Hops reuse the profile's password/key
(one credential per profile). Two Docker integration tests (password- and key-auth,
target on an internal-only network reachable only via the bastion) plus IPv6/parse
unit tests. **ProxyCommand** and `ssh_config ProxyJump` import remain as follow-ups.

**What & why.** Reach a target that isn't directly routable by chaining through one
or more bastions. Many corporate networks *only* allow SSH via a jump host — without
this, that whole audience can't connect. (Roadmap C3.)

**References.** OpenSSH `-J`/`ProxyJump` (7.3+), internally a nested `ssh -W`
riding a `direct-tcpip` channel through the bastion; multi-hop is a comma list
visited in order ([ssh_config(5)](https://man.openbsd.org/ssh_config), RFC 4254 §7.2).
UI: Termius "Host Chain", PuTTY ProxyCommand (`%host`/`%port`), SecureCRT/Xshell
per-session firewall/jump dropdown. OSS: **russh issue #182** documents the exact
recipe — `session.channel_open_direct_tcpip(target, port, "127.0.0.1", 0).await?.into_stream()`
yields a `ChannelStream` (AsyncRead+AsyncWrite), then
`russh::client::connect_stream(config, stream, handler)` runs the next handshake
over it. Adit already uses `channel_open_direct_tcpip(...).into_stream()` for `-L`/`-D`
tunnels (`adit-ssh/src/lib.rs` ~2033–2044), so the primitive is battle-tested here.

**Approach for Adit.** Data model: add an optional `jumps: Vec<Hop>` to the profile
(serde default so old files load). Connect: hop 0 via `client::connect` (own TCP);
each further hop opens `direct-tcpip` to the next target and runs `connect_stream`
over that channel; **each hop is a real handshake, so the target's host key is
verified end-to-end through the tunnel.** Keep **all** intermediate `Handle`s alive
for the session lifetime (drop one and its channel dies). Route SFTP and tunnels
through the **same** final session (today `SftpRequest` dials host:port directly —
it must accept a pre-chained session or rebuild the chain). ProxyCommand as a fast
follow-up on the same `connect_stream` transport (spawn a process, join
stdin/stdout as the stream; expand `%h`/`%p`/`%r`). Import `ssh_config`
`ProxyJump`/`ProxyCommand` onto the model.

**Pitfalls (all confirmed by research).** ① Verifying the **wrong** host key — each
hop needs a `KnownHostsClient` keyed to **that hop's** host:port, or you silently
trust the wrong key. ② Dropping intermediate `Handle`s kills the tunnel. ③ SFTP
bypassing the chain (own TCP dial) fails on non-routable targets. ④ Host-key prompts
must serialize per hop and cancellation must unwind the whole chain. ⑤ Keepalive on
the **final** session only; a per-hop connect timeout so one dead bastion fails fast.
⑥ ProxyCommand token/quoting on Windows is an injection/connect-failure vector.

**Acceptance.** (a) one-jump profile opens a shell on the target and known_hosts
gains/verifies **both** bastion and target; (b) two-hop chain (A→B→target) connects
in one click, in order, each with its own creds; (c) tampering the **target** key
prompts labeled as the target and rejecting tears down the chain; (d) per-hop auth
(bastion key/agent + target password, and the reverse); (e) SFTP + one forward on a
jumped profile reach a non-routable target; (f) importing `Host t / ProxyJump a,b`
yields a 2-hop chain; profiles without `jumps` load unchanged.

---

## 4. Interactive MFA (keyboard-interactive multi-prompt)

**Status: ✅ shipped in v0.1.32.** adit-ssh emits `LiveShellEvent::AuthPrompt`
(`AuthPromptRequest{name, instructions, prompts:Vec<AuthPromptField{prompt,echo}>}`)
and takes `LiveShellCommand::AuthResponses(Vec<String>)`. The keyboard-interactive
driver (`keyboard_interactive_round_answers`) auto-fills account-password fields
(`should_autofill_password`: masked-or-labelled, excluding second factors and
new-password prompts) from the stored password and asks the user for anything
else (e.g. a verification code), across multiple rounds; the shell path pumps the
command channel in a `tokio::select!` so answers/disconnect arrive mid-handshake,
and a cancel aborts the whole connect (`AuthenticationCancelled`, no key/agent
fallback). adit-session bridges it like the host-key prompt
(`pending_auth_prompt`/`respond_auth_prompt`); adit-ui shows a modal with one
input per field (masked when `echo=false`). SFTP/tunnel/jump-hops keep the
non-interactive heuristic. Unit-tested (password vs code split, multi-round,
mixed round, cancel, non-interactive fallback). **Follow-up:** a real
PAM/google-authenticator Docker e2e test (deferred — TOTP is time-based/flaky;
the plan scoped this as "best-effort").

**What & why.** Today keyboard-interactive is only a password fallback that
auto-answers **every** server prompt with the saved password via a keyword
heuristic (`adit-ssh/src/lib.rs:922–969`, `keyboard_interactive_answer` at 1099).
Real MFA — TOTP (Google Authenticator), Duo, RSA SecurID, "password expired → set
new password" — needs the server's **live** prompts displayed and per-prompt answers
typed, sometimes over multiple rounds. Note: the roadmap's "MFA-aware" claim is
overstated; this is the real fix. (Matters mainly for the enterprise audience.)

**References.** RFC 4256 (keyboard-interactive): the server sends a name +
instruction + a list of prompts each with an **echo** flag, possibly across multiple
`InfoRequest` rounds. PuTTY/OpenSSH render the instruction and one input per prompt,
masked when echo=false. russh API:
`authenticate_keyboard_interactive_start` / `..._respond`, iterating `InfoRequest`.

**Approach for Adit.** Mirror the existing host-key prompt bridge. adit-ssh: add
`LiveShellEvent::KeyboardInteractivePrompt(KbdIntPrompt{name, instruction, fields:
Vec<KbdIntField{prompt, echo}>})` and `LiveShellCommand::KeyboardInteractiveResponse(Vec<String>)`.
Refactor `authenticate_password_or_keyboard_interactive` into a real driver: on each
`InfoRequest`, if `prompts` is empty respond `vec![]` immediately; otherwise emit the
event and await the user's answers over a per-round oneshot, then call `..._respond`.
Because auth runs before the main channel loop, wrap it in a `tokio::select!` that
also drains `commands` for the response/disconnect (same shape as the connect-phase
select). adit-ui: a small dialog like the host-key one, one input per field, masked
when `echo=false`, supporting >1 round. Fall back to the saved password only for a
single non-echo "password"-ish prompt (preserve today's one-factor behavior).

**Pitfalls.** Never log codes. Support multiple rounds (Duo/TOTP often 2). Don't feed
the password to a prompt that clearly wants a code. Time-box user input so a stuck
prompt doesn't hang the connect.

**Acceptance.** (a) a TOTP server (password then "Verification code:") logs in with
the user typing the code; (b) a 2-round exchange works; (c) echo=false masks input,
echo=true shows it; (d) "password expired → new password" completes; (e) a plain
single-password keyboard-interactive server still works with the saved password;
(f) codes never appear in logs.

---

## 5. Key passphrase (distinct field) + `.ppk` support

**What & why.** Today the encrypted-key passphrase reuses the single login-password
field (`authenticate_with_private_key_and_hash`, `adit-ssh/src/lib.rs` ~1035),
**silently swallows load errors** (`Ok(false)`), and never handles PuTTY `.ppk` —
the exact format Adit's Windows/Pageant audience carries. (Roadmap A4 / deferred #46.)

**References.** Key formats a client must handle: OpenSSH new format
(`-----BEGIN OPENSSH PRIVATE KEY-----`), classic PEM (PKCS#1 RSA / SEC1 EC), PKCS#8,
and PuTTY `.ppk` v2 (SHA-1 MAC) / v3 (Argon2id KDF). `russh::keys::load_secret_key`
covers OpenSSH/PEM/PKCS#8/SEC1 but not `.ppk`.

**Approach for Adit.** Phase 1 (small, high value): split passphrase from password.
Add `passphrase: Option<String>` to the request/auth structs and a **passphrase field**
in the profile editor + connect dialog (next to the existing identity-file picker).
Pass the passphrase (not the login password) to `load_secret_key`; on `Err`
distinguish "encrypted key needs passphrase" / "wrong passphrase" from other errors
via a typed `SshError` so the UI can re-prompt instead of silently continuing. Phase 2
(`.ppk`): an internal `ppk` module parsing `PuTTY-User-Key-File-2/-3`, verifying the
`Private-MAC`, deriving keys (v2 SHA-1; v3 Argon2id via the `argon2` crate using the
file's params), converting to a russh key.

**Acceptance.** (a) an encrypted OpenSSH/PEM key connects with the passphrase in its
own field; (b) a wrong passphrase yields a clear "wrong passphrase" re-prompt, not a
silent fallthrough; (c) the passphrase is distinct from the login password in UI and
storage; (d) a `.ppk` v2 and v3 key load and authenticate; (e) a corrupt/MAC-mismatch
`.ppk` errors clearly.

---

## 6. Per-session appearance (color-code prod/staging)

**What & why.** Let a profile carry its own accent/tab color, optional background
tint, and a short label/badge (e.g. "PROD"), so an operator can tell at a glance
which server a tab/pane is. Directly targets the "ran it on the wrong server"
mistake that iTerm2, SecureCRT, Termius, and Windows Terminal all address via
per-profile color.

**References.** iTerm2 badges + per-profile colors; SecureCRT per-session color/
window; Termius; Windows Terminal per-profile `background`/`tabColor`.

**Approach for Adit.** Domain: add an optional appearance to `ConnectionProfile`
(`adit-domain`) — an `environment: Environment` enum (None|Dev|Staging|Prod|Custom)
+ `accent_color: Option<String>` (hex) + `label: Option<String>`, all
`#[serde(default)]` (old `profiles.json` loads unchanged). The enum gives
mistake-proof red/amber/green presets with Custom as the escape hatch; optional
`background_tint: Option<f32>` (0.0–0.15 overlay). Rendering is the real work:
today the palette is a single process-global atomic `TERM_SCHEME` (set once in
`view()`), which can't express per-pane overrides — resolve the scheme/accent
**per session** (per tab, per split pane) instead of a single global, and tint the
tab + optional terminal background.

**Acceptance.** (a) a profile can set an environment/accent/label, persisted;
(b) the tab and (optionally) terminal background reflect it; (c) split panes each
show their own session's color; (d) old profiles render exactly as today; (e) a
"PROD" label is visible on the tab.

---

## 7. Windows code signing (Authenticode)

**What & why.** The unsigned exe + installer hits SmartScreen's "Windows protected
your PC" wall on every release and never builds reputation (Smart App Control can
block outright). Signing with a cert chained to the Microsoft Trusted Root Program
shows a verified publisher and accumulates reputation across releases. (Roadmap-adjacent; #44.)

**References.** SmartScreen judges publisher-cert reputation + per-file-hash
reputation; unsigned starts at zero every release. Options in 2026: OV vs EV certs,
and **Azure Trusted Signing** (cloud, ~$10/mo, no hardware token, CI-friendly).

**Approach for Adit.** Adopt **Azure Trusted Signing** as the primary path — cheapest
fully-cloud option, no USB dongle, built for CI. The release pipeline is already
GitHub-Actions-driven: after the Rust build and Inno Setup packaging, `azure/login`
via OIDC federated credentials, then the official `azure/trusted-signing-action`
(endpoint + account + certificate-profile, `file-digest SHA256`, RFC3161 timestamp
`http://timestamp.acs.microsoft.com`). Sign `adit-app.exe` **and** the installer.

**Acceptance.** (a) the signed exe/installer show a verified publisher in the UAC/
properties dialog; (b) a fresh download no longer triggers the SmartScreen wall (or
clears it quickly as reputation builds); (c) signing runs in CI on tag without
secrets in the repo (OIDC); (d) the timestamp makes signatures outlive cert expiry.

**Note.** This needs an Azure Trusted Signing account (org identity, ~$10/mo) — a
prerequisite the user must set up; the code/CI work is small once it exists.

---

## 8. OSC 8 hyperlinks + URL detection (click to open)

**What & why.** Make links clickable: (1) OSC 8 explicit hyperlinks
(`ESC ] 8 ; params ; URI ST`), and (2) heuristic detection of bare `http(s)://…`.
Hover-underline; Ctrl/Cmd+click opens after a confirmation showing the real
destination (matches this project's link-safety standard). (Roadmap B3.)

**References.** The [OSC 8 spec](https://gist.github.com/egmontkob/eef283d3fb00c8f3f5e8);
WezTerm / iTerm2 / Windows Terminal implementations; WezTerm's default URL regexes.

**Approach for Adit.** Terminal core (`adit-terminal/src/vt.rs`): add a `b"8"` arm to
`osc_dispatch` (rejoin fields 2.. with `;` to tolerate URIs with semicolons; parse
`id=`); add `link: u16` to `Pen` and an interning table (`Vec<Arc<str>>` + de-dupe)
on the state — `put_char` already copies `pen` into each cell, so no per-char cost;
add `link: Option<String>` to `TerminalCell`, included in run-coalescing equality so
a link boundary starts a new run. UI (`adit-ui`): heuristic regex over each rendered
line for bare URLs; hover-underline linked runs; Ctrl/Cmd+click → the existing
link-safety confirmation → open in the browser.

**Acceptance.** (a) an OSC 8 link (e.g. from `ls --hyperlink=auto`, `gcc` diagnostics)
is clickable and opens the right URI; (b) a bare `https://…` in output is detected
and clickable; (c) the confirmation shows the real destination before opening;
(d) links survive scrollback and reflow; (e) non-link text is unaffected.

---

## 9. Zmodem (rz/sz) — deferred

**What & why.** ZMODEM transfers files in-band over the existing shell channel: the
client watches for the ZMODEM init header, takes over the channel with a progress UI,
then returns to the terminal. For Adit this is **parity/convenience, not a capability
gap** — the SFTP panel is strictly more capable (browse, recursive dirs). ZMODEM's
value is workflow (type `sz file`/`rz` at any prompt, including through sudo /
jump-host / nested shells). Top SecureCRT/Xshell parity request after SFTP, but
**high risk** (in-band protocol takeover). **Recommendation: defer**; revisit on
explicit demand.

**References/approach (for when it's picked up).** Hook a sentry at
`run_live_password_shell` (`adit-ssh/src/lib.rs` ~706–749) where `ChannelMsg::Data`
becomes `Output`: scan for the `**`+ZDLE+`B00` ZRQINIT trigger; on detection emit
`LiveShellEvent::ZmodemDetected{direction}` (do **not** auto-confirm) → UI prompt
(save dir / file picker) → switch the loop into a transfer state feeding channel data
into a `zmodem2` Receiver/Sender, streaming files on a blocking thread, emitting
progress. Study `lrzsz` and the `zmodem2` crate.

---

## Non-goals (unchanged)

Telnet and a full plugin system remain out of scope. (Local Shell / Serial / RDP
already landed despite the earlier non-goal note.)

# Local draft closeout, 2026-11-05

This is a dated recovery record requested by the repository owner, not a roadmap.
GitHub Issues remain the source for feature scope and acceptance. The baseline
for this audit is `39315c4c4a5cacbffdd43398ff035a7367e27ce3`.

Recovered implementation commits:

- `7b5f9c3`: SVC reassembly bounds and helper output budget/termination.
- `2bacf5a`: native window-owned SFTP file drops and directory-reply ownership.
- The accompanying closeout commit adds this record, the original handoff,
  the helper regression gate and local compiler-output ignores.

## Recovery sources

| Source | Contents | Disposition |
| --- | --- | --- |
| `stash` object `d0d17e0f1236ff88358ac96726a6f18743d04333` | Native OLE file-drop experiment | Adapted to current routing, including queue consumption and lifetime checks; not restored as a whole tree. |
| Its third parent `430dc8c1ad63824a52a429fe96db3f72b9269174` | Previously untracked `native_file_drop.rs` | Required recovery source; a normal tracked-only stash patch omits it. |
| `e1be4026c0510ba3f7ef9d9eeb45b82f623d7f17` | Clipboard revocation investigation | Independent SVC and output-budget hardening recovered. Obsolete policy IPC and local-file upload experiments superseded by current safety decisions. |
| `issue110-link-visual` worktree patch | Hyperlink render fixtures | Recovered by `39315c4`, with theme-aware palette selection. |

The original snapshots are retained. Recovery does not mean every historical
line should execute in the current product. In particular, local file sharing
to RDP remains disabled and the current clipboard barriers remain authoritative.

Recovery tags (archive namespace, not releases):

- `archive/drafts/2026-11-05/native-file-drop` preserves the entire original stash,
  including its untracked-file parent.
- `archive/drafts/2026-11-05/rdp-revocation` preserves `e1be402`.
- `archive/drafts/2026-11-05/hyperlink-fixture` preserves the old worktree's exact
  uncommitted fixture patch (`e5ed642`). That worktree is now clean; the adapted
  implementation was already pushed in `39315c4`.

Use `git show <tag>` to inspect a snapshot. Do not apply a whole historical tree
on top of main: it includes obsolete protocol and dependency configurations.

## Already integrated historical work

The previous audit examined 22 non-main branches and six detached worktree
heads. Some source commits were rewritten while integrating, so Git's raw
`--no-merged` output is not a list of missing features.

| Historical work | Main integration evidence |
| --- | --- |
| SFTP `69046c7` | `3132548` |
| SSH authentication `7427f0f` | `03cbf28` |
| ProxyCommand `1c440ed` | `1f5d595` |
| RDP pointers/layouts `ab13f9b` | `473a155` |
| Appearance `df1d93c` | `e30854a` |
| Detached windows `1f433b3` | `58dad44` |
| SSH transport-close work `d8140fd` / `b7a4ee8` | `04a2182` / `8e415b0` |
| VNC gateway deadlines `6baac3a` / `5a4807b` | `d371a4c` / `800c803` |
| Clipboard format serialization | `9026a0e` (PR #228) |
| Ambiguous drop rejection | `46c0109` (PR #229) |
| Workbench `3487b29`, links `137ddbc`, dialogs `f6fd408`, display recovery `796c4de` | Already ancestors of main |

The audit also found the integrated catalog, i18n, terminal, tunnel,
accessibility, Linux file bridge and remote OLE drag-out implementations.
Do not restore older versions over these paths merely to obtain commit ancestry.

## Integration and validation

### SVC reassembly

The dependency patch caps each assembled message at 128 MiB, validates declared
and accumulated lengths, reserves for received bytes and releases state after
malformed input. It keeps complete unflagged and LAST-only messages, empty LAST,
and fresh FIRST resynchronization. Header-only SUSPEND/RESUME controls return no
application message and preserve in-flight assembly; they are not errors.
Compressed content remains unsupported and is rejected before dispatch. Existing
session-wide outbound suspend/resume coordination is not supplied by this patch.

The helper explicitly registers `svc_dechunkify` and runs 21 regression tests,
including published Microsoft wire examples. The original stricter draft was
changed after independent review caught its handling of valid flow controls.
See `crates/adit-rdp/IRONRDP-PATCHES.md` for protocol links and provenance.

### Helper output

The output budget is 2048 ordinary messages / 320 MiB allocated payload, including
the current write and flush, with a single reserved 16 KiB Error plus Closed tail.
Exceeding the budget ends the RDP session rather than blocking input releases or
protocol responses behind stdout. Active writes are checked every 100 ms and end
the session after 10 seconds without progress. Final draining is bounded by
30 seconds total and 10 seconds without progress; runtime shutdown is bounded by
one second. It uses a duplicate stdout OS handle so a blocked write cannot hold
Rust's exit-time StdoutLock. Already transmitted data cannot be recalled; a bad
pipe need not deliver the terminal tail.

This is a queue/payload budget, not a total process RSS limit: one encoded frame
also has a temporary serialization allocation. IPC version and wire enums remain
unchanged. The new lifecycle regressions include a real helper process with no
stdout reader, which exits itself in about 11 seconds, without a fake helper
mode. Both `just ci` and Windows CI now run the helper tests rather than merely
building/linting them.

### Native file drop

Windows OLE screen points are converted to logical client coordinates and hit
against the receiving window's published regions. The first legal hit binds the
drag to a window, session or panel instance, directory and revision. A later
target/context/geometry/modal change invalidates that drag permanently until a
fresh DragEnter, including A-to-B-to-A transitions. Accepted batches are consumed
by the production update loop and revalidated before dispatch. Unknown or
overlapping targets, local drops onto RDP, shell batches with multiple paths,
and directories remain rejected. Existing overwrite confirmation is preserved.

SFTP browser and shell identities now require a connected state and no
outstanding navigation. An initial shell Listing/Error is retired separately;
cd, ls and completion replies match the requested path. This also fixes the
race where an initial listing could prematurely complete cd and leave drops
targeting the previous directory. Revision exhaustion fails closed. Queue
storage is bounded to 4096 paths / 4 MiB of UTF-16 path content per batch and
32 requests / 16 MiB path content overall.

Registration failure produces a visible refusal and leaves file-picker upload
available; it does not silently fall back to ambiguous window-level routing.
Non-Windows retains the existing winit receive path. The acceptance executable
requires fresh isolated configuration and environment directories and has a
20-second watchdog. The production iced window registered its OLE target
successfully in both worker and lead-model smoke runs (`registered=true`, exit 0).

Focused regressions passed: 35 file-drop tests (one existing ignored test) and
11 session native-drop tests. Independent review caught and verified fixes for
disconnected identities and the initial-listing/cd race. These tests include
real COM calls and actual window-origin/DPI conversion plus the production
queue/update loop; they do not represent Explorer cross-process interoperability
or real SSH file consumption. Issue #94's direct bidirectional RDP drag/drop
scope and the disabled local-file-to-RDP path remain separate.

### Final validation

The combined Windows tree passed:

- `just ci`: workspace build, root/helper all-targets Clippy with warnings as
  errors, workspace tests, all helper tests and SSH integration compilation.
- `cargo clippy -p adit-ui --all-targets --features native-vnc-acceptance,native-workbench-acceptance,native-accessibility-acceptance --locked -- -D warnings`.
- The corresponding UI library tests: 380 passed, six ignored. These overlap
  the normal workspace tests; they are not additional unique test cases.
- The lead-model isolated production iced registration run: exit 0,
  `registered=true`, `refusal_visible=false`.
- Root/helper format checks and staged/unstaged diff checks.

SVC/output and native-routing changes received separate read-only reviews; the
identified control-PDU and SFTP reply-ownership defects were corrected and
rechecked. The shared implementation was then frozen for the combined gate.

Linux runners were offline during this closeout. The local Ubuntu WSL lacks
Linux GUI build prerequisites, so no full Linux build is claimed. The
non-Windows receive path remains winit-based and Windows-only entry points are
conditionally compiled, but runner CI is still required to confirm that path.
Explorer cross-process drag/drop and real SSH transfer acceptance were not run.

Native GUI, real server interoperability, CI and published-release status are
distinct: source recovery does not certify every historical issue or ship a
release. Original stash entries are retired only after their remote archive
tags and the recovered implementation have been verified. Historical branches
and detached-worktree references may remain as recovery history; they are not
pending implementation work.

## Non-code local artifacts

Repository-local `target-*` directories contain compiler output, including
generated Rust files, and are ignored. `adit-workspace-concept.png` is a local
design artifact, not an omitted implementation. It is retained locally.

The handoff is now tracked at
[issue-acceptance-2026-11-05.md](issue-acceptance-2026-11-05.md). Its environment
and service observations are dated; they are not a current health guarantee.

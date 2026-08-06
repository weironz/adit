# Roadmap — done

Shipped work, newest first. Kept rather than deleted because the reason a thing
was built the way it was is the part that gets lost, and because several entries
here are the only record of a failure that cost real time.

For what is *not* built, see [roadmap.md](roadmap.md). For how it works, see
[features.md](features.md) and [architecture.md](architecture.md).

---

## v0.1.66

**Telnet.** A first-class protocol beside SSH / SFTP / RDP. IAC negotiation
answers ECHO, SUPPRESS-GO-AHEAD, TERMINAL-TYPE and NAWS, and refuses everything
else *explicitly* — an unanswered option leaves some servers waiting forever.
It replies only on a real state change, which is the guard that stops two
implementations acknowledging each other in a loop. Not yet verified against
real hardware (see [roadmap.md](roadmap.md)).

**GitHub Gist over OAuth device flow.** Was a pasted personal access token.
Device flow is the one GitHub flow needing no client secret and no loopback
listener. Worth recording *why*, because the obvious reason is wrong: GitHub
**does** support PKCE (added July 2025), but it does not distinguish public from
confidential clients, so the web flow still demands a client secret and PKCE
buys a desktop app nothing here. Pasting a token still works, for networks where
a browser round trip does not.

**Protocol pills.** The tile says what kind of machine a host is; the pill says
how you reach it. Two separate questions, and the same Ubuntu box answers both —
a wall of identical orange cards could not tell SSH from RDP. Telnet's pill is
red: it is the one protocol here with no encryption, and the badge was already
being drawn.

**Real logos.** Ubuntu, Windows, Red Hat, Alpine and four generic marks as
compiled-in SVG, replacing geometric glyphs. A new profile takes an icon from
its protocol — RDP means Windows, everything else starts at Ubuntu — because a
field nothing ever filled meant the logos were invisible by default.

**Group icons.** A `group_icons` map beside the group list rather than turning
`groups` into structs: the list carries order and membership that thirty-odd
call sites read as plain names. No format migration, on a file whose last
rewrite lost a whole catalog. Three places would have silently dropped the
icons — the merge, the save path, and `SyncFinished` — each looking like the
feature never worked rather than like a bug.

**SFTP as a protocol.** Reaching a host's files no longer means opening a shell
you did not want. It dials its own connection, which is the point and also its
one caveat: MFA hosts still need the ride-along path.

**SFTP shell: tab completion and history.** Completes commands, then local or
remote paths depending on which argument of which command. Only the prefix every
candidate shares is inserted; where that adds nothing the candidates are listed,
because a Tab that appears to do nothing is the worst of the three outcomes.
Doing this first required fixing the arrow keys, which were being typed into the
line as `[A`.

**RDP dirty rectangles.** Was one rect grown around everything that changed, so
a blinking cursor and a clock in opposite corners shipped most of the screen.
Now up to sixteen, merging only when merging is *free* — overlapping or tiling
edge to edge — which is what lets a full repaint's hundreds of adjacent tiles
collapse back to roughly one rect while unrelated activity stays apart.

**Reconnect stops asking for a password** it already had in the credential
store.

---

## v0.1.65

**Cloud sync.** Six providers behind one trait: GitHub Gist, WebDAV,
S3-compatible, Google Drive, OneDrive, Dropbox. Per-session three-way merge
keyed by UUID against a stored ancestor. Design and provider quirks in
[cloud-sync.md](cloud-sync.md).

Two of its rules exist because breaking them destroyed a real 152-session
catalog, and both are load-bearing:

- **The ancestor only advances to a state read back and confirmed.** Without it
  a lost race deletes a session on the *recovery*, not on the race.
- **An absent remote document is not a deletion.** Deleting the file from a
  provider's web UI — a reasonable way to say "start over" — was read as "the
  other machine deleted all 152 sessions", and that deletion propagated home.
  Behind it sits a brake: a sync whose result would empty a populated machine is
  refused outright.

**One settings page.** 应用 / 外观 / 终端 / 日志 / 同步与云 behind a category
rail, replacing three separate dialogs that meant three places to look for one
setting. Every setting is its own card.

**English translation.** Lookup keyed by the Chinese source string, falling back
to it — every string here was written in Chinese first, so English is a
translation of it, and a missing entry shows the original rather than a bare
key. The language lives in an atomic rather than on `AditApp`, because the
alternative is threading it through every widget helper in the crate for a value
that never differs between them.

**Toolbar removed**, and the tab strip hidden when idle. Every toolbar button
was already a menu item.

---

## Earlier

**H.264 for RDP** — AVC420, then AVC444 written from scratch (two AVC420 views
recombined into persistent per-surface YUV 4:4:4 planes). The wire is Annex-B,
not the length-prefixed NALs the library's documentation describes.

**The RDP scroll-ghosting hunt.** Root cause was the tile-application rule:
fresh passes (TILE_FIRST/TILE_SIMPLE) must blit whole, upgrade passes must clip
to the region rects. FreeRDP clips both, which is provably wrong for fresh
passes on a Windows stream. Proven from captured bytes rather than reasoned
about — see [rdp-debugging.md](rdp-debugging.md), which exists because two
plausible theories were refuted by the capture before the real one was found.

**RemoteFX Progressive decoding**, without which a healthy session renders a
solid black desktop — a symptom that looks like a connection bug and is not.

**Auto-login and fullscreen for RDP.** Microsoft-account hosts need the local
SAM name for interactive logon even though NLA accepts the email alias.

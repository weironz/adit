# Keyword highlighting — design

Status: **built.** Kept because the reasoning outlived the plan — and because two
of its conclusions turned out to be wrong, which is recorded below rather than
quietly edited away.

Colour terminal output locally, by pattern, without the server's help. SecureCRT
calls this Keyword Highlighting; MobaXterm ships something similar. For a client
aiming at SecureCRT's ergonomics it is table stakes, and it is the one class of
readability win that still works on a machine you are not allowed to install
anything on.

## The problem is narrower than it looks

A screenshot of a "nicely coloured" session usually contains two unrelated
things, and conflating them leads straight to a bad feature.

**`ls` in colour is not our doing.** `ls --color` emits SGR escapes and the
terminal renders them. Adit already does this; there is nothing to add — and,
more importantly, nothing to *touch*.

**`cat somefile` in colour cannot come from `cat`.** It emits no escapes at all.
Either the shell aliases `cat` to `bat`, or the client is painting it. Only the
second is this feature.

So the scope is exactly: **text the server sent with no colour of its own.**

## The constraint everything else follows from

> Never repaint a cell the server coloured.

Server colour carries meaning we cannot reconstruct: blue is a directory, red is
an archive, green and red are the two sides of a diff, yellow is a level the
remote tool already classified. A local rule painting over it destroys
information and looks like a rendering bug. Getting this wrong makes the feature
net-negative, which is why it is stated before anything else.

The test already exists in the model: `TerminalCell.fg == Color::Default`
([crates/adit-terminal/src/lib.rs](../crates/adit-terminal/src/lib.rs)) means the
pen was at SGR 39 when the cell was written. No new flag is needed. Cells whose
`fg` is anything else are off limits.

Bold-without-colour is the one grey area — `resolve()` in
[vt.rs](../crates/adit-terminal/src/vt.rs) brightens *indexed* colours under
bold, but a bold cell with a default pen is still `Color::Default`. Treat it as
paintable: bold is emphasis, not classification.

## Where it plugs in

The UI layer, beside scrollback search — not the terminal core.

`search_matches` in [crates/adit-ui/src/lib.rs](../crates/adit-ui/src/lib.rs)
already overlays highlight onto rendered lines, so this is a second consumer of
an existing mechanism rather than new machinery. Keeping it out of
`adit-terminal` also keeps the emulator honest: the grid stays a faithful record
of what the server sent, and highlighting stays a property of the view.

One consequence is worth stating up front. `render_row` coalesces cells into runs
keyed by `(fg, bg, attrs, link)`. A highlight covering part of a run has to split
it, so highlighting must be applied *during* that coalescing, never by patching a
finished run.

**Precedence, leftmost wins:**

```
selection  >  search match  >  keyword rule  >  server colour  >  scheme default
```

Selection and search are transient and user-initiated, so they win. Between
overlapping keyword rules, first match in list order wins and the list is
user-orderable: "first rule wins" is predictable, "most specific wins" is not.

## What ships

- A rule is `{ pattern, colour, enabled, whole-word? }`. Regex, not glob.
- Rules apply only to `Color::Default` cells, only on the **primary screen**.
- Matching is per line, over the visible viewport only.
- A small built-in default set (below), plus user-added rules.
- Rules live in the profile store beside colour schemes, so they travel with a
  profile instead of being a global-only preference.
- Settings UI strings in Simplified Chinese, matching the rest of the app.

## What does not ship

**Per-language syntax highlighting.** Not "colour this `.env` like an `.env`
file". A terminal sees a byte stream with no file type; inferring one from
`cat foo.env` means parsing the command line, tracking shell state, and being
wrong the moment anyone pipes something.

> **Corrected.** This section originally rejected syntax highlighting outright,
> and that was too broad. A screenshot of MobaXterm colouring `cat test.py` —
> keywords, strings, comments — showed the middle path this argument had missed:
> a *language-agnostic* rule set is context-free, fits the engine unchanged, and
> gets most of the way there. `code-string`, `code-keyword` and `code-number`
> ship because of it. What stays rejected is only the per-language half.

**The alternate screen.** `vim`, `less`, `htop` and `tmux` paint every cell
themselves and own their layout. Local highlighting there is noise at best and
misleading at worst — a rule firing inside vim's status line. Detect the
alternate screen and suppress highlighting wholesale.

**Mutating the scrollback.** Highlighting is render-time only. Copy, paste,
selection export and search keep seeing the original text and the original
colours. Someone pasting fabricated colour into a bug report is a failure mode we
decline to have.

**Stateful or multi-line rules.** No "inside a heredoc", no "until the closing
brace". Every rule matches within one line, independently of every other line.
That is also what makes the viewport-only optimisation correct: a line's
appearance must not depend on lines that have scrolled away.

**Prompt detection.** Echoed input is indistinguishable from output at this
layer, so a rule can fire on half of what someone is typing as they type it.
Accepted as a known wart. Working around it means recognising prompts, which is a
rabbit hole with no reliable bottom.

## The default set, and why it is small

Defaults are on for everyone, so a wrong default is a bug shipped to every user.
Only patterns that are context-free *and* rarely coincidental qualify:

| Rule | Pattern (sketch) | Why it is safe |
|---|---|---|
| Errors | `\b(ERROR\|FATAL\|CRITICAL)\b` | Unambiguous in any output, and catching the eye is the entire point |
| Warnings | `\b(WARN\|WARNING)\b` | Same |
| IPv4 | `\b\d{1,3}(\.\d{1,3}){3}\b` | Structurally distinctive, and useful in exactly the sessions Adit is for |
| URLs | `\bhttps?://\S+` | Already special-cased for hyperlinks elsewhere |

> **Corrected twice over.** The set is nine now, all of them on, and the entry
> below was the load-bearing mistake. Anchoring the comment pattern to
> line-start defeats both objections it was rejected for — a root prompt has
> `root@host:/path` in front of its `#`, and `+++`/`---` are not `#` at all —
> and neither was re-examined once written down. The rules also needed a
> *scope*: colouring only where a `#` sits reads as a stray fragment, which no
> amount of tuning the colour fixes. iTerm2 splits its highlight trigger into
> "text" and "line" for exactly that reason.
>
> They ship on because the user asked for them on, having seen both. The cost is
> real and stated where the rule is defined: `if`, `for` and `else` are ordinary
> English, so prose in log output picks up colour. The dialog is how anyone who
> minds turns that one off.

**Originally shipped off by default:**

- `#` comments. Lovely on `cat somefile`; wrong on a log line containing a `#`,
  on `+++`/`---` in a diff, and on a root prompt.
- `$`-prefixed words. Would have shredded the bcrypt hash
  `$$2y$$05$$Yr3ee…` in the screenshot that prompted this design into three
  differently coloured fragments.
- Timestamps, file paths, hex and UUIDs. Useful, but with high false-positive
  rates on exactly the dense output where noise hurts most.

When a default is in doubt, ship fewer. Users who want more can switch them on;
users surprised by wrong colour on first run cannot un-see it.

## Performance

Regex over the viewport only — tens of lines per frame, not the tens of thousands
a scrollback holds. Compile patterns once when they change, never per frame.
Cache per line, keyed by the line's revision, so an idle screen re-renders
without re-matching.

User-supplied regex needs a bound: a linear-time engine, or a complexity cap at
compile time, so a pasted pathological pattern cannot hang the UI thread. That
matters more here than it would elsewhere — [CLAUDE.md](../CLAUDE.md) records
that blocking the UI thread reads to users as "Not Responding", with no crash and
no log to explain it.

## Suggested order

1. Rule engine, `Color::Default` gating, alternate-screen suppression, default
   set hardcoded. Self-contained and already useful.
2. Settings UI and profile persistence.
3. Presets, per-rule toggles, ordering.

Step 1 carries all the design risk. If precedence or the never-repaint rule turns
out to be wrong, that is far cheaper to discover before a settings surface exists
to be migrated.

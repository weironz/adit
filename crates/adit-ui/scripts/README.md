# UI translation audit

No Rust build, external packages, configuration files or credentials are needed.
Run from the repository root:

```powershell
node crates/adit-ui/scripts/check-i18n.mjs
node crates/adit-ui/scripts/check-i18n.mjs --inventory
node --test crates/adit-ui/scripts/check-i18n.test.mjs
```

The first command fails on missing catalog entries, duplicate sources within a
catalog/context, placeholder drift, Chinese in English translations, and direct
Chinese literals passed to widget, dialog or error constructors without lookup.
It distinguishes `t`/`tf` from `t_in`/`tf_in`: a context-only entry cannot satisfy
a generic call. `--inventory` emits JSON with source path, line, decoded literal
and coverage, suitable for reviewing additions after merging another branch.

The lexer handles comments, nested block comments, escaped/multiline/raw Rust
strings, character literals and `#[cfg(test)]` modules. Optional acceptance
harnesses are excluded. This is a lexical check, not a Rust AST or a reachability
proof: review indirect label helpers, conditional text, native dialogs and
formatted data as part of UI testing.

`COMMON`, `CONTEXTUAL` and `RUNTIME` in `src/i18n.rs` are the source of truth.
Keep Chinese source text as the key. Use `t_in("field", source)` for a different
UI meaning, `tf(source, args)` for positional templates, and the GUI's `runtime`
adapter for engine diagnostics. Runtime `{error}` slots explicitly allow nested
diagnostic translation; every other capture stays opaque. Named engine
placeholders are normalized only by the audit, not by modifying engine code.

The producer list also checks domain labels and SessionOptions, session/RDP/
trigger diagnostics, catalog/SSH-config/PuTTY imports and credential protection.
When a producer or field-label combination is added, update the list or catalog.
Exclusions are explicit and justified in the script: stored profile-copy names
and engine terminal transcripts are user data/terminal bytes; terminal rendering
must never rewrite them. User names, commands, paths, badges supplied by the user,
remote output, authentication challenges and OS-owned dialog text remain intact.

The Rust regressions in `src/i18n/tests.rs` avoid changing the process-global
language, so parallel UI tests cannot race each other's selected language.
They cover lookup fallback, ambiguous meanings, positional data, nested errors,
unknown diagnostics, Windows paths and real SessionOptions validation.
The coordinating agent should run these serially with its other Rust checks:

```powershell
cargo test -p adit-ui i18n::tests
cargo clippy --workspace --all-targets -- -D warnings
```

Also smoke-test English and Chinese UI with disposable configuration: session
editor/options, menus, SFTP queue, VNC diagnostics, RDP monitor controls, import
warnings, credential protection and sync confirmation. Long English labels may
need layout review; already-open native dialogs follow their creation language
and OS-owned controls follow the operating system.

The native `workbench_acceptance` example accepts
`ADIT_WORKBENCH_LANGUAGE=zh` or `en` and records the language in its report.
Its capture sequence includes the session editor and settings as well as D/E/F.
Use a fresh empty temp configuration and screenshot directory for every run;
see the native validation runbook for the remaining fixture variables.

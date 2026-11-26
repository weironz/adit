# Adit patch for ironrdp-cliprdr 0.7.0

Based on the published [ironrdp-cliprdr 0.7.0 crate](https://crates.io/crates/ironrdp-cliprdr/0.7.0).
The `.crate` archive's SHA-256 is
`cb9050999a1e032f4313788ac5a6d06e897a4f672f1de2ed98683001fc1abf56`.
The retained `.cargo_vcs_info.json` identifies IronRDP commit
`11a0810cfbbabd8b8023875a05e3041216d4b01b`, path `crates/ironrdp-cliprdr`.
Everything except `src/lib.rs` and this file is the archive verbatim (its
`.cargo-ok` marker and `Cargo.lock` are dropped). The crate is MIT OR
Apache-2.0; see both license texts.

One `ADIT PATCH` hunk in `Cliprdr::request_file_contents`: a RANGE request
with no `clipDataId` may end exactly one byte past the advertised file size.
Every other bound check is unchanged. The rationale and the tests that cover it
are in [the helper patch map](../../IRONRDP-PATCHES.md).

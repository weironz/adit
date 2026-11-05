# Adit patch for ironrdp-svc 0.8.0

Based on the published [ironrdp-svc 0.8.0 crate](https://crates.io/crates/ironrdp-svc/0.8.0).
The `.crate` archive's SHA-256 is
`24c36b82ab0f7fef2668fb7004008a0f3c100a3d9a7b18bd4495a71aba55b796`.
The retained `.cargo_vcs_info.json` identifies IronRDP commit
`11a0810cfbbabd8b8023875a05e3041216d4b01b`, path `crates/ironrdp-svc`.
The generated and original manifests, README, changelog and license files come
from that archive (changelog trailing whitespace and an extra blank line at the
end of the original manifest are trimmed). The crate is
MIT OR Apache-2.0; see both license texts.

Only `src/lib.rs` is patched, for bounded receive reassembly and error cleanup;
`tests/dechunkify.rs` is new. The old detached `e1be402` draft informed the limit,
length checks and cleanup, but its stricter flag grammar was not carried over.
No experimental CLIPRDR patch or helper IPC changes are part of this vendor.

Header-only SUSPEND/RESUME controls are consumed without dispatching application
data or changing reassembly state. This preserves pending fragments, including
across controls carrying FIRST/LAST. It does not implement the existing missing
session-wide outbound pause/resume coordination. Compressed SVC data remains
explicitly unsupported. The policy, protocol references and limits are documented
in [the helper patch map](../../IRONRDP-PATCHES.md).
Tests run as the helper's explicit `svc_dechunkify` target, despite the published
vendor manifest disabling library tests and automatic integration discovery:

```powershell
cargo test --manifest-path crates/adit-rdp/Cargo.toml --locked --test svc_dechunkify
```

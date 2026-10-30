# Adit patch for ironrdp-svc 0.8.0

This directory is based on the published `ironrdp-svc` 0.8.0 crate from
<https://crates.io/crates/ironrdp-svc/0.8.0>. The downloaded `.crate` archive has
SHA-256 `24c36b82ab0f7fef2668fb7004008a0f3c100a3d9a7b18bd4495a71aba55b796`.
Its `.cargo_vcs_info.json` identifies IronRDP commit
`11a0810cfbbabd8b8023875a05e3041216d4b01b` and source path
`crates/ironrdp-svc`. Upstream's generated `Cargo.toml`, `Cargo.toml.orig`,
README, changelog, and both license texts are retained. Whitespace in the
changelog and original manifest is normalized. The package is licensed
under MIT OR Apache-2.0; see `LICENSE-MIT` and `LICENSE-APACHE`.

## Local change

`src/lib.rs` bounds reassembly to 128 MiB per static channel and per message,
checks fragment boundaries and repeated uncompressed lengths before copying,
and releases the partial allocation on every reassembly error. It never
preallocates the claimed message length. The limit allows a raw 7680 x 4320 x
4-byte image, while the Adit clipboard consumer has its own smaller image limit.
As allowed by the receiving rules in
[MS-RDPBCGR 3.1.5.2.2](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/334495b7-47b9-4e91-8d08-05d46285799b),
a complete, length-matching unflagged PDU is accepted when no fragment sequence
is active.

The current Adit configuration does not advertise bulk compression. IronRDP's
SVC path does not decompress channel data before this crate sees it, and the
header length refers to uncompressed data; compressed SVC chunks are therefore
rejected. If SVC decompression is implemented upstream, revisit the compression
flag handling and validate the limits on decompressed output. Flow-control
flags, peer interoperability, and channels that legitimately need a single PDU
larger than 128 MiB have not been independently validated. The bound is per
channel, not a process-wide budget or a timeout on an unfinished PDU; allocator
overhead and later copies can add to peak memory use.

## Integration and tests

The helper's workspace must add `ironrdp-svc = { path = "vendor/ironrdp-svc" }`
under its existing `[patch.crates-io]`, refresh its own lockfile, and verify
that all users resolve to this copy. A direct dependency alone is insufficient to
guarantee that transitive IronRDP consumers use the same implementation.

`tests/dechunkify.rs` exercises the public `StaticVirtualChannel::process` path.
The upstream generated manifest sets `autotests = false`, so integration must
register this test explicitly in the helper's manifest:

```toml
[[test]]
name = "svc_dechunkify"
path = "vendor/ironrdp-svc/tests/dechunkify.rs"
```

`tests/chunk_processor_unit.rs` checks private state and is included only when
this crate's library unit tests are explicitly selected with `cargo test --lib`;
simply testing the helper as a dependency does not run it. Currently, invoking
the vendor manifest in place fails because it is inside the helper workspace
but is not a member. An isolated copy can be tested independently; another
option is to add `members = [".", "vendor/ironrdp-svc"]` to the helper's
existing `[workspace]`, then run
`cargo test --manifest-path crates/adit-rdp/Cargo.toml -p ironrdp-svc --lib --locked`
explicitly in CI. Test registration and workspace membership were checked in a
temporary workspace, not in the shared helper manifest. Both suites use
synthetic bytes and do not access a system clipboard or a remote desktop.

At the time of this patch, `ironrdp-session` propagates SVC decoding failures to
the helper's `finish_active_process`, which skips a failed PDU unless there is
also an EGFX decode error. Integration should add a regression using that real
chain and prove that a malformed clipboard SVC PDU is followed by a healthy
graphics update and another channel's traffic. Recheck this behavior if the
helper's error handling changes.

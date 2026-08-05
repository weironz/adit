# Cloud sync

Sessions, groups and settings, merged across machines. Six providers behind
one small trait, with the care concentrated in the one place that can lose a
user's work: the merge.

Code lives in [`crates/adit-sync`](../crates/adit-sync). This document is the
design and the operator's manual — what travels, why the merge is built the way
it is, and the one-time registration each OAuth provider needs.

---

## 1. What travels, and what does not

| | Merged how |
|---|---|
| **Sessions and groups** | Per item, by UUID (see §2) |
| **Settings** | Whole value, last writer wins |
| **Saved passwords** | Whole blob, opt-in, already encrypted |

Settings are deliberately *not* merged per field: there is no meaningful
reconciliation of "font size" between two machines, and pretending otherwise
would produce a result neither machine asked for.

Credentials travel only as the sealed XChaCha20-Poly1305 blob that
`adit_storage::credentials` already writes to disk. **The passphrase never
leaves the machine.** A provider breach yields ciphertext; moving to a new
machine costs one passphrase entry. The option is off by default — it is safe,
but it is the user's call.

The document on the provider is plain, pretty-printed JSON, and that is a
requirement rather than a convenience: a user must be able to open their own
Gist or Drive file, see what Adit put there, and recover it by hand if the app
is unavailable.

---

## 2. Why the merge needs an ancestor

Two catalogs alone cannot be merged correctly. "Present here, absent there" is
both *added on this side* and *deleted on that side*, and those demand opposite
actions. Without a third reference the only safe reading is a union, which
never deletes anything the user deleted.

So every successful sync records the catalog it produced, and the next merge
diffs both sides against it. **Timestamps are deliberately not an input**:
clocks on two machines disagree, and a restored backup carries a stale one.

The rules, each with a test in `merge.rs`:

| Situation | Result |
|---|---|
| Added on both sides | Both kept |
| Edited on one side only | That edit applies, no conflict |
| Deleted on one side, untouched on the other | Deletion propagates |
| Deleted on one side, **edited** on the other | Edit wins, reported as a conflict |
| Edited differently on both sides | Local kept, remote handed back, reported |
| Edited identically on both sides | Not a conflict |
| No ancestor at all (first sync) | Union |

Losing the ancestor file is survivable: the merge falls back to a union, which
over-keeps rather than deletes.

Ordering follows the local machine's arrangement, with remote-only entries
appended. Sorting by `sort_order` would look tidier and would silently rewrite
a drag-and-drop order the user set by hand.

---

## 3. The read-back rule

**The ancestor only ever advances to a state read back from the provider and
confirmed to be ours.**

This is the load-bearing rule of the whole feature, and the reason is not
obvious. Only some providers offer a conditional write. Without one, two
machines syncing at once can both fetch revision 1, both merge, and the second
push silently discards the first.

The trap is assuming that heals by itself. It does not:

```
A pushes v2 (contains A's host)     B overwrites with v3 (does not)
A syncs again:
    ancestor = v2  → has A's host
    remote   = v3  → does not
    three-way merge reads that as "the other machine deleted it"
    → A's host is removed for good
```

**The loss lands on the recovery, not on the race.** So after every push the
orchestrator reads back; if what returns is not what it wrote, the ancestor
stays exactly where it was and the whole sync runs again. Measured from the
untouched ancestor, our sessions are still *additions*, and additions are never
dropped.

The read-back runs for every provider, not only the ones lacking `If-Match` —
one extra GET is cheap next to explaining where a host went.
`racing_writer_does_not_lose_our_work` in `orchestrate.rs` pins this.

---

## 4. Providers

| Provider | Conditional write | Setup | Notes |
|---|---|---|---|
| **GitHub Gist** | ✗ none | A token with `gist` scope | Version history free; a secret gist is URL-readable, not private |
| **WebDAV** | ✓ `ETag` / `If-Match` | URL + password | Nextcloud, 坚果云, Synology. The safest of the six |
| **S3-compatible** | ✓ (2024+) | Access key pair | AWS, MinIO, R2, 阿里云 OSS. Hand-written SigV4 |
| **Google Drive** | ✗ none | OAuth (§5) | `drive.file`: only files this app created |
| **OneDrive** | ✓ `if-match` | OAuth (§5) | `Files.ReadWrite.AppFolder` |
| **Dropbox** | ✓ `rev` | OAuth (§5) | App folder. Best conflict story of the three drives |

Providers without a conditional write are safe because of §3, not because the
race cannot happen.

### Traps already paid for

- **Gist** truncates inline content past ~1 MB and expects the client to follow
  `raw_url`; a large session list reaches that.
- **Gist** deleted from the web UI reads as empty and re-creates itself rather
  than wedging sync forever.
- **Gist** mints its id on the *first push*; forgetting it scatters sessions
  across a growing pile of gists. `SyncBackend::assigned_id` carries it back.
- **WebDAV** servers answer `405`, not `404`, for a path that never existed.
- **S3** signs the *encoded* path — a key with a space signs differently from
  one with `%20` written out.
- **S3** path-style and virtual-host addressing produce different canonical
  paths and Host headers; getting it backwards fails every request identically.
- **S3** buckets that deny `ListBucket` answer `403` for a missing key rather
  than leak whether it exists, so both codes mean "nothing stored yet".
- **OneDrive** `/content` redirects to a storage host carrying no eTag, so the
  metadata must be fetched separately or there is nothing to make the next
  write conditional on.
- **Dropbox** without `autorename: false` "resolves" a conflict by inventing
  `adit-sync (1).json`, which nobody would ever look in again.
- **Google** without `prompt=consent` stops issuing a refresh token to a
  *reconnecting* user, who then works for an hour and stops.

---

## 5. Registering the OAuth applications

One-time, by the maintainer. **Users register nothing** — they click authorize
in their browser and their data goes to *their* account. The client id
identifies the application; the access token identifies the user, and the two
never mix.

All three are PKCE public clients: **no client secret is needed or used.**
Google issues one for desktop app types and its own documentation concedes it
is not confidential; sending it would add nothing but the pretence of
protection.

### Google Drive

[Google Cloud Console](https://console.cloud.google.com/apis/credentials) →
API 和服务 → 凭据 → 创建凭据 → OAuth 客户端 ID

1. **启用 Google Drive API**：左栏 **Library** → 搜 "Google Drive API" →
   **Enable**。少了这步，授权会成功而所有 API 调用被拒 —— 一个看起来像
   "凭据错了" 的失败。
2. **声明权限范围**：左栏 **OAuth consent screen**（不在 Credentials 页里）。
   旧版界面是 编辑应用 → 第二步 **范围**；改版后的 Google Auth Platform 把它
   放在 **数据访问**。搜 `drive.file` 勾选
   **`https://www.googleapis.com/auth/drive.file`** —— 仅本应用创建的文件，
   是能用的最小权限。
3. **创建客户端**：Credentials → 创建凭据 → OAuth 客户端 ID，应用类型
   **桌面应用**。拿 **客户端 ID**（形如
   `xxx.apps.googleusercontent.com`）。**不需要客户端密钥。**

The scope that is actually requested comes from the client — our authorize URL
carries `scope=...drive.file`. What the console holds is the *declaration*,
which is what decides the consent screen wording and whether verification
applies. The console also states which tier a scope falls into; trust it over
any summary here, including this one, because the policy moves.

### OneDrive

[Azure portal (Entra ID)](https://entra.microsoft.com/) → 应用注册 → 新注册

- 支持的账户类型：选含 **「个人 Microsoft 账户」** 的那项
- 重定向 URI：平台选 **「移动和桌面应用程序」**，填 `http://localhost`
- API 权限：Microsoft Graph → 委托的权限 →
  **`Files.ReadWrite.AppFolder`** + **`offline_access`**
- 拿 **应用程序(客户端) ID**

### Dropbox

[Dropbox App Console](https://www.dropbox.com/developers/apps) → Create app

- 选 **Scoped access** → **App folder**
- Permissions 页勾 **`files.content.write`** + **`files.content.read`**
  （勾完要点 **Submit**，未提交的勾选不生效）
- 拿 **App key**

---

## 6. Shipping the client ids

Build-time environment variables, read with `option_env!` in
`backend/mod.rs::client_id`:

```
ADIT_SYNC_GOOGLE_CLIENT_ID
ADIT_SYNC_ONEDRIVE_CLIENT_ID
ADIT_SYNC_DROPBOX_CLIENT_ID
```

Set them as CI secrets on the release workflow. A build without them leaves
those three providers unconfigured — the panel says so rather than failing at
the browser.

That includes **your own local build**, which is the surprising part: `just app`
compiles with whatever is in the environment, and a developer machine has none
of these set, so a locally built Adit shows Google Drive, OneDrive and Dropbox
as unconfigured even on the machine where the ids were registered. Copy
[`.cargo/config.toml.example`](../.cargo/config.toml.example) to
`.cargo/config.toml` (gitignored) and fill them in — Cargo applies `[env]` to
every build in the workspace and tracks the values, so changing one recompiles
the crate that reads it. No `build.rs` is needed: `rustc` records `option_env!`
lookups in the dep-info file and Cargo rebuilds on them, which is worth knowing
before adding a build script to "fix" a rebuild that already works.

**Users can override any of them.** That is not decoration: a shared client id
is a shared API quota. rclone ships one for Drive and is retiring it during
2026 for exactly that reason, telling users to create their own. Having the
escape hatch from the start beats adding it under pressure — and it is also
what keeps local and forked builds usable, since only the release pipeline sets
the variables above.

The registration is a single point of failure worth naming: **disabling or
deleting an app registration breaks sync for every user of that provider.**
Gist, WebDAV and S3 are unaffected — those carry the user's own credentials.

---

## 7. Testing

Unit tests (`cargo test -p adit-sync`) cover the merge, the orchestration
including the racing-writer case, SigV4 against AWS's published vectors, and
PKCE against the RFC 7636 vector.

An end-to-end check against the real GitHub API lives in `tests/gist_live.rs` —
ignored, because it needs a token and creates a gist on whichever account owns
it:

```bash
GITHUB_TOKEN=$(gh auth token) \
  cargo test -p adit-sync --test gist_live -- --ignored --nocapture
```

It covers what unit tests structurally cannot: that a first push genuinely
creates a gist and returns an id, that a second machine's sessions merge rather
than replace, and that the id is reused instead of minting a fresh gist every
sync. Each run leaves one secret gist behind, deliberately — deleting it would
also delete the evidence someone ran this to look at.

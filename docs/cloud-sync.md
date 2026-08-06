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

### An absent remote is not a deletion

The read-back guards the *push*. It says nothing about the *fetch*, and the
fetch had the worse bug:

```
provider holds no document  →  treated as "the remote is an empty catalog"
                            →  three-way merge against a populated ancestor
                            →  "the other machine deleted all 152 sessions"
                            →  deletion propagates home, machine wiped,
                               emptiness pushed back up
```

Deleting the file from the provider's web UI — a reasonable way to say "start
over" — was enough to trigger it, and it destroyed a real 152-session catalog.
So does switching providers (the new one is empty while the ancestor is not),
signing into a different account, and any provider that answers "not found" for
a reason other than absence.

**No document means there is no other side, so nothing can have deleted
anything.** The ancestor is dropped for that attempt and local is treated as
new — the first-sync shape, which unions rather than deletes.

Behind it sits a brake: **a sync whose result would empty a populated machine is
refused outright.** Propagating a deletion is right for one session and
indefensible for all of them, and being wrong once is unrecoverable. Two tests
in `orchestrate.rs` hold both halves, and both fail if either is removed.

---

## 4. Providers

| Provider | Conditional write | Setup | Notes |
|---|---|---|---|
| **GitHub Gist** | ✗ none | OAuth device flow (§5), or a pasted `gist` token | Version history free; a secret gist is URL-readable, not private |
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
- **GitHub's OAuth endpoints answer form-encoded by default**, not JSON —
  `device_code=...&user_code=...`. Without `Accept: application/json` the parse
  fails and reports itself as malformed JSON, which reads like a GitHub outage
  rather than a missing header.
- **GitHub's device flow is off until switched on** in the OAuth App's settings.
  Until then the device-code request answers `200` with
  `error=device_flow_disabled` — a success status carrying a refusal, which is
  also why the device-code response has to be checked for an `error` key
  instead of trusting the HTTP status.
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
- **Dropbox** matches `redirect_uri` against its registered list literally, with
  no loopback exception — the RFC 8252 "let the OS pick a port" approach that
  Google and Microsoft both accept fails on every attempt. It needs a pinned,
  pre-registered port (53682, following rclone).
- **Windows** `cmd /C start "" <url>` truncates any URL at its first `&`: Rust
  quotes an argument only when it holds whitespace, so a percent-encoded URL
  arrives bare and `&` reads as a command separator. An authorize URL is
  nothing but `&`-joined parameters, and the provider reports it as a *missing
  client id*. Hand URLs to `rundll32 url.dll,FileProtocolHandler` instead — one
  argv, no shell.
- **Google** without `prompt=consent` stops issuing a refresh token to a
  *reconnecting* user, who then works for an hour and stops.

---

## 5. Registering the OAuth applications

One-time, by the maintainer. **Users register nothing** — they click authorize
in their browser and their data goes to *their* account. The client id
identifies the application; the access token identifies the user, and the two
never mix.

Google, Microsoft and Dropbox are PKCE public clients over a loopback redirect.
Dropbox and Microsoft need **no client secret**, and none is sent. GitHub is a
different shape — see below.

**Google is the exception, and its documentation is wrong about it.** The table
for installed apps lists `client_secret` as *optional*; the token endpoint then
answers `400 invalid_request: "client_secret is missing."` and the authorisation
dies after the browser has already said "已授权". So a Google desktop client has
to ship its secret, the way rclone has for years — compiled in, plainly not
confidential, and protecting nothing that PKCE was not already protecting. Take
the server's word over the table's.

### Why GitHub uses the device flow instead

Not because GitHub lacks PKCE — **it has supported it since July 2025**, and the
folklore that it does not is out of date. The reason is the sentence next to
that announcement: GitHub **does not distinguish public from confidential
clients**. PKCE is optional on every GitHub flow, and the web application flow's
token exchange still requires `client_secret` regardless. PKCE there is defence
in depth for a client that already ships a secret — it does not turn a desktop
app into a public client the way it does on Dropbox and Microsoft.

The device flow is the one GitHub flow that needs **no client secret** (GitHub
says so outright) and **no loopback listener**. That is worth more here than the
extra browser polish: nothing confidential is compiled into a binary users
already have, and there is no localhost port to bind — which is the part that
fails first on a locked-down corporate machine.

Its cost is that the user must transcribe an 8-character code, and there is no
redirect back to close the loop. So the panel keeps the code on screen for the
whole polling window, in monospace and next to a copy button. A device flow
whose code is hidden is a device flow nobody can finish.

**Pasting a token by hand is still supported and should stay that way.** The
browser half is the part most likely to be unavailable — `github.com/login` is
blockable, and an operator holding a fine-grained token should not have to
authorise an OAuth app to use it. Both paths end in the same sealed credential
slot, and `backend/gist.rs` cannot tell them apart by design.

### GitHub (Gist)

[GitHub Developer settings](https://github.com/settings/developers) → OAuth Apps
→ New OAuth App

- **Application name** 会显示在授权页上，用户看到的就是这个名字
- **Homepage URL** 填仓库地址即可
- **Authorization callback URL** 是必填项，但设备流程**根本不会用到它** ——
  填仓库地址或 `http://localhost` 都行，GitHub 只是不允许留空
- 建好后进入应用设置页，勾上 **Enable Device Flow** 并保存。**少了这一步，
  申请设备码会返回 `device_flow_disabled`** —— 一个看起来像 client id 填错了
  的失败，实际上只是这个开关没开
- 拿 **Client ID**。**不要**生成 client secret：设备流程不需要，桌面端也存不住

权限范围不在这里声明。GitHub 的 scope 由客户端在请求里指定，Adit 只申请
`gist` —— 读写用户自己的 gist，碰不到仓库，也没有更小的粒度可选。

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
   **桌面应用**。**客户端 ID 和客户端密钥两个都要拿**（见上文：Google 的文档说
   密钥可选，它的服务器说不可选）。
4. **改应用名称**：同意页上显示给用户的名字来自 OAuth consent screen 的
   「应用名称」。若这个项目是从别的用途沿用来的，用户会看到那个旧名字在申请
   权限 —— 一件足以让人取消授权的小事。

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
- **OAuth2 Redirect URIs 填 `http://localhost:53682/` 并点 Add** —— 结尾的斜杠
  不能少。Dropbox 逐字比对这个字符串，且**不给 loopback 端口开例外**，所以
  Adit 对它固定用 53682 端口，而不像 Google / Microsoft 那样由系统随机分配。
- Permissions 页勾 **`files.content.write`** + **`files.content.read`**
  （勾完要点 **Submit**，未提交的勾选不生效）
- 拿 **App key**

---

## 6. Shipping the client ids

Build-time environment variables, read with `option_env!` in
`backend/mod.rs::client_id`:

```
ADIT_SYNC_GOOGLE_CLIENT_ID
ADIT_SYNC_GOOGLE_CLIENT_SECRET
ADIT_SYNC_ONEDRIVE_CLIENT_ID
ADIT_SYNC_DROPBOX_CLIENT_ID
ADIT_SYNC_GITHUB_CLIENT_ID
```

Only Google has a secret, for the reason in §5. A user supplying their own
Google client id must supply its secret too — the panel has a field for both,
and neither needs a rebuild. GitHub takes no secret at all: the device flow is
specified not to need one.

Set them as CI secrets on the release workflow. A build without them leaves
those providers unconfigured — the panel says so rather than failing at the
browser. **Gist is the one that degrades rather than goes dark:** without
`ADIT_SYNC_GITHUB_CLIENT_ID` the 连接账号 button is disabled, but pasting a
personal access token still works, because that path never needed a client id.

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

The device-flow polling state machine is tested against captured response
bodies rather than the network (`backend/device.rs`), because its four
documented answers are exactly where this is easy to get wrong: two of them mean
*keep waiting* (`authorization_pending`, `slow_down`) and two mean *stop*
(`expired_token`, `access_denied`). Collapsing the pairs produces either a flow
that quits while the user is still typing, or one that polls a dead code until
the app closes. The tests assert all four stay distinct, and that `slow_down`
actually widens the interval rather than merely not failing.

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

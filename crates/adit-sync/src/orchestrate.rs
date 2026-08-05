//! Driving one sync: fetch, merge, push, and — the part that makes it safe —
//! read back before believing it.
//!
//! ## Why the read-back exists
//!
//! Only some providers offer a conditional write. WebDAV and modern S3 do
//! (`If-Match`); GitHub Gist does not. Without one, two machines syncing at
//! once can both fetch revision 1, both merge, and the second push silently
//! discards the first.
//!
//! The trap is assuming that heals on its own. It does not. Say A pushes and
//! B overwrites it. On A's next sync, A's ancestor still contains A's profile
//! and the remote no longer does — which is precisely the shape of "the other
//! machine deleted it", so a correct three-way merge removes it for good. The
//! loss happens on the *recovery*, not on the race.
//!
//! So the ancestor is only ever advanced to a state read back from the
//! provider and confirmed to be ours. If the read-back disagrees, the ancestor
//! stays where it was and the whole thing runs again: with the old ancestor,
//! our profiles are still *additions* relative to it, and additions survive.
//! That is the entire mechanism, and `racing_writer_does_not_lose_our_work`
//! is the test that holds it in place.
//!
//! ## What is merged and what is not
//!
//! Sessions and groups merge per item (see [`crate::merge`]). Settings do not:
//! there is no meaningful per-field merge of "font size" between two machines,
//! so the last writer wins, and that is stated rather than hidden. Credentials
//! travel as one sealed blob for the same reason — it is opaque ciphertext,
//! and anything other than whole-blob replacement would corrupt it.

use std::path::{Path, PathBuf};

use adit_storage::ProfileCatalog;
use serde::{Deserialize, Serialize};

use crate::merge::{three_way, Conflict, MergeStats};
use crate::{RemoteRevision, SyncBackend, SyncDocument, SyncError};

/// How many times to re-merge when another machine keeps winning the race.
/// Three is plenty: each attempt costs a round trip, and a provider busy
/// enough to lose three in a row will still be there in a minute.
const MAX_ATTEMPTS: usize = 3;

/// What the last confirmed sync agreed on. The ancestor every merge is
/// measured against.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncState {
    /// The catalog as the provider last confirmed it. Never a local guess.
    #[serde(default)]
    pub base: ProfileCatalog,
    #[serde(default)]
    pub revision: RemoteRevision,
    /// RFC 3339, for the status panel only.
    #[serde(default)]
    pub last_synced_at: String,
}

/// Where the ancestor lives. Losing this file is survivable — the merge falls
/// back to a union, which over-keeps rather than deletes — so it sits next to
/// the profiles rather than anywhere precious.
pub struct SyncStateStore {
    path: PathBuf,
}

impl SyncStateStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A missing or unreadable file reads as "never synced", which is the safe
    /// default: an empty ancestor makes the next merge a union.
    #[must_use]
    pub fn load(&self) -> SyncState {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state: &SyncState) -> Result<(), SyncError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| SyncError::Malformed(error.to_string()))?;
        std::fs::write(&self.path, bytes)?;
        Ok(())
    }
}

/// Everything the caller needs after a sync.
#[derive(Debug, Clone)]
pub struct SyncOutcome {
    /// What the caller should now hold locally, and save.
    pub catalog: ProfileCatalog,
    /// Sessions the merge could not settle. Local was kept in each case; the
    /// discarded side rides along so the UI can offer it.
    pub conflicts: Vec<Conflict>,
    pub stats: MergeStats,
    /// Whether anything was actually uploaded. `false` means both sides were
    /// already identical.
    pub uploaded: bool,
}

/// What to send alongside the catalog. Both are whole-value replacements.
#[derive(Debug, Clone, Default)]
pub struct Extras {
    pub settings: Option<adit_storage::AppSettings>,
    /// The sealed credential file, hex-encoded. Ciphertext only — the
    /// passphrase never leaves the machine.
    pub credentials: Option<String>,
}

fn same_catalog(a: &ProfileCatalog, b: &ProfileCatalog) -> bool {
    serde_json::to_string(a).unwrap_or_default() == serde_json::to_string(b).unwrap_or_default()
}

/// Nothing in it at all — no sessions and no groups. Both halves matter: a
/// catalog holding only empty groups is still not something worth wiping a
/// populated machine for.
fn catalog_is_empty(catalog: &ProfileCatalog) -> bool {
    catalog.profiles.is_empty() && catalog.groups.is_empty()
}

/// Run one sync to completion.
///
/// `state` is read and, on success, advanced. `local` is this machine's
/// current catalog. The returned catalog is what the caller must save — it may
/// differ from `local` because the remote had changes.
pub fn sync(
    backend: &mut dyn SyncBackend,
    store: &SyncStateStore,
    local: &ProfileCatalog,
    extras: &Extras,
    device: &str,
    now: &str,
) -> Result<SyncOutcome, SyncError> {
    let state = store.load();
    let mut last_conflict: Option<SyncError> = None;

    for _ in 0..MAX_ATTEMPTS {
        let fetched = backend.fetch()?;
        let (remote_catalog, remote_revision) = match &fetched {
            Some((document, revision)) => (document.catalog.clone(), Some(revision.clone())),
            None => (ProfileCatalog::default(), None),
        };

        // "The provider holds nothing" is the shape of a FIRST sync, not of a
        // remote that deleted everything — and the difference is the entire
        // catalog. Measured against a populated ancestor, an absent document
        // reads as "the other machine deleted all 152 sessions", that deletion
        // propagates home, and the machine is wiped. There is no other side to
        // have deleted anything, so the ancestor is dropped for this attempt
        // and local is simply new.
        //
        // This is not hypothetical. Deleting the file from the provider's web
        // UI — a reasonable way to say "start over" — did exactly this, and so
        // do switching providers, signing into a different account, and any
        // provider that answers "not found" for a reason other than absence.
        let no_ancestor = ProfileCatalog::default();
        let base = if fetched.is_some() {
            &state.base
        } else {
            &no_ancestor
        };

        // Always merged against the ancestor from BEFORE this loop started.
        // Re-reading `state` inside the loop would let a half-finished attempt
        // become the ancestor, which is the failure this whole design avoids.
        let merged = three_way(base, local, &remote_catalog);

        // The brake. Propagating a deletion is deliberate (see the merge
        // rules), but a sync that empties a populated machine is a different
        // animal: every deletion in that result came from the other side, and
        // being wrong once is unrecoverable. Refusing costs a puzzled user;
        // proceeding costs them everything.
        if catalog_is_empty(&merged.catalog) && !catalog_is_empty(local) {
            return Err(SyncError::Remote {
                provider: backend.provider().to_owned(),
                message: String::from(
                    "同步结果会清空本机全部会话，已中止。请先确认云端数据是否正常。",
                ),
            });
        }

        // Both sides already agree: record the ancestor and stop. This still
        // advances `base`, which is correct — the provider just told us what it
        // holds, so it is confirmed by definition.
        if same_catalog(&merged.catalog, &remote_catalog) && same_catalog(&merged.catalog, local) {
            store.save(&SyncState {
                base: merged.catalog.clone(),
                revision: remote_revision.unwrap_or_default(),
                last_synced_at: now.to_owned(),
            })?;
            return Ok(SyncOutcome {
                catalog: merged.catalog,
                conflicts: merged.conflicts,
                stats: merged.stats,
                uploaded: false,
            });
        }

        let mut document =
            SyncDocument::new(merged.catalog.clone(), device.to_owned(), now.to_owned());
        document.settings = extras.settings.clone();
        document.credentials = extras.credentials.clone();

        match backend.push(&document, remote_revision.as_ref()) {
            Ok(_) => {}
            // A provider with compare-and-swap telling us we were beaten. Not
            // an error to show anyone — go round again against what is there
            // now, still measured from the untouched ancestor.
            Err(error @ SyncError::Conflict { .. }) => {
                last_conflict = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        }

        // The read-back. For providers without conditional writes this is the
        // only thing standing between a lost race and a deleted session, so it
        // runs for every provider rather than only the ones that need it — one
        // extra GET is cheap next to explaining where a host went.
        match backend.fetch()? {
            Some((document, revision)) if same_catalog(&document.catalog, &merged.catalog) => {
                store.save(&SyncState {
                    base: merged.catalog.clone(),
                    revision,
                    last_synced_at: now.to_owned(),
                })?;
                return Ok(SyncOutcome {
                    catalog: merged.catalog,
                    conflicts: merged.conflicts,
                    stats: merged.stats,
                    uploaded: true,
                });
            }
            // Someone wrote between our push and our read. Leave the ancestor
            // exactly where it was and try again: our sessions are still
            // additions relative to it, and additions are never dropped.
            _ => continue,
        }
    }

    Err(last_conflict.unwrap_or(SyncError::Conflict {
        provider: backend.provider().to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use adit_domain::ConnectionProfile;

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile::new(name, format!("{name}.example"), 22, "will")
    }

    fn catalog(profiles: Vec<ConnectionProfile>) -> ProfileCatalog {
        ProfileCatalog::new(Vec::new(), profiles)
    }

    fn state_store() -> (SyncStateStore, tempdir::TempDir) {
        let dir = tempdir::TempDir::new();
        let store = SyncStateStore::new(dir.path().join("sync-state.json"));
        (store, dir)
    }

    /// A provider that can be told to misbehave in the two ways that matter.
    #[derive(Default)]
    struct Fake {
        remote: Option<SyncDocument>,
        revision: u32,
        /// Overwrite the remote with this immediately after each push, which
        /// is exactly what a racing second machine looks like from here.
        race_after_push: Option<ProfileCatalog>,
        /// Refuse the next N pushes with a conflict, as a conditional write
        /// would when another writer got there first.
        refuse: usize,
        pushes: usize,
    }

    impl SyncBackend for Fake {
        fn provider(&self) -> &'static str {
            "Fake"
        }

        fn fetch(&mut self) -> Result<Option<(SyncDocument, RemoteRevision)>, SyncError> {
            Ok(self.remote.clone().map(|document| {
                (
                    document,
                    RemoteRevision {
                        token: self.revision.to_string(),
                    },
                )
            }))
        }

        fn push(
            &mut self,
            document: &SyncDocument,
            _expected: Option<&RemoteRevision>,
        ) -> Result<RemoteRevision, SyncError> {
            if self.refuse > 0 {
                self.refuse -= 1;
                return Err(SyncError::Conflict {
                    provider: "Fake".to_owned(),
                });
            }
            self.pushes += 1;
            self.revision += 1;
            self.remote = Some(document.clone());
            if let Some(racer) = self.race_after_push.take() {
                self.revision += 1;
                self.remote = Some(SyncDocument::new(racer, "other".into(), "now".into()));
            }
            Ok(RemoteRevision {
                token: self.revision.to_string(),
            })
        }
    }

    /// First sync against an empty provider: everything local goes up and the
    /// ancestor is recorded.
    #[test]
    fn a_first_sync_uploads_and_records_the_ancestor() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("web")]);
        let mut backend = Fake::default();

        let outcome = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "2026-08-05T02:30:00Z",
        )
        .expect("sync");

        assert!(outcome.uploaded);
        assert_eq!(outcome.catalog.profiles.len(), 1);
        assert_eq!(store.load().base.profiles.len(), 1);
        assert_eq!(store.load().last_synced_at, "2026-08-05T02:30:00Z");
    }

    /// Nothing changed anywhere: no upload, but the ancestor is still confirmed
    /// from what the provider reported.
    #[test]
    fn an_unchanged_sync_uploads_nothing() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("web")]);
        let mut backend = Fake {
            remote: Some(SyncDocument::new(local.clone(), "other".into(), "now".into())),
            revision: 7,
            ..Fake::default()
        };
        store
            .save(&SyncState {
                base: local.clone(),
                revision: RemoteRevision {
                    token: "7".to_owned(),
                },
                last_synced_at: "earlier".to_owned(),
            })
            .expect("seed");

        let outcome = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "now",
        )
        .expect("sync");

        assert!(!outcome.uploaded);
        assert_eq!(backend.pushes, 0);
    }

    /// The one that matters. Another machine overwrites the remote in the
    /// window between our push and our read-back. Without the read-back the
    /// ancestor would advance to a state the provider never kept, and our
    /// session would read as "deleted remotely" next time and vanish.
    #[test]
    fn racing_writer_does_not_lose_our_work() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("mine")]);

        let mut backend = Fake {
            // Their push lands right after ours and does not contain our host.
            race_after_push: Some(catalog(vec![profile("theirs")])),
            ..Fake::default()
        };

        let outcome = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "now",
        )
        .expect("sync");

        // The second attempt merged against the untouched ancestor, so our
        // host is still an addition and survives alongside theirs.
        let names: Vec<_> = outcome
            .catalog
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(names.contains(&"mine"), "our session was lost: {names:?}");
        assert!(names.contains(&"theirs"));
        // And the ancestor now matches what the provider actually holds.
        assert!(same_catalog(&store.load().base, &outcome.catalog));
    }

    /// A conditional-write refusal is retried rather than surfaced, and the
    /// ancestor is untouched while that happens.
    #[test]
    fn a_conditional_write_refusal_is_retried() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("web")]);
        let mut backend = Fake {
            refuse: 1,
            ..Fake::default()
        };

        let outcome = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "now",
        )
        .expect("sync");

        assert!(outcome.uploaded);
        assert_eq!(backend.pushes, 1, "the refused attempt must not count");
    }

    /// A provider that never lets us win gives up with a conflict rather than
    /// looping, and crucially leaves the ancestor alone so nothing is lost.
    #[test]
    fn an_endless_race_gives_up_without_touching_the_ancestor() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("web")]);
        let mut backend = Fake {
            refuse: MAX_ATTEMPTS,
            ..Fake::default()
        };

        let error = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "now",
        )
        .expect_err("should give up");

        assert!(matches!(error, SyncError::Conflict { .. }));
        assert!(store.load().base.profiles.is_empty());
    }

    /// Remote-only additions come back to the caller so it knows to save them.
    #[test]
    fn a_remote_addition_is_returned_for_saving() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("mine")]);
        let remote = catalog(vec![profile("theirs")]);
        let mut backend = Fake {
            remote: Some(SyncDocument::new(remote, "other".into(), "now".into())),
            revision: 1,
            ..Fake::default()
        };

        let outcome = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "now",
        )
        .expect("sync");

        assert_eq!(outcome.catalog.profiles.len(), 2);
        assert_eq!(outcome.stats.added_from_remote, 1);
    }

    /// Deleting the file from the provider's web UI is a reasonable way to say
    /// "start over". It must not be read as "the other machine deleted every
    /// session", which is what an absent document looks like when it is
    /// measured against a populated ancestor — that wiped a real machine's 152
    /// sessions and pushed the emptiness back up.
    #[test]
    fn deleting_the_remote_document_does_not_delete_the_sessions() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("web"), profile("db")]);
        // A previous successful sync: the ancestor knows about both.
        store
            .save(&SyncState {
                base: local.clone(),
                revision: RemoteRevision::default(),
                last_synced_at: "then".to_owned(),
            })
            .expect("seed the ancestor");
        // ...and now the provider holds nothing.
        let mut backend = Fake::default();

        let outcome = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "now",
        )
        .expect("sync");

        assert_eq!(outcome.catalog.profiles.len(), 2, "nothing may be dropped");
        assert!(outcome.uploaded, "the sessions must be pushed back up");
        assert_eq!(store.load().base.profiles.len(), 2);
    }

    /// The brake: a remote that genuinely holds an empty catalog would, by the
    /// deletion rule, empty this machine too. That rule is right for one
    /// session and indefensible for all of them, so it is refused instead.
    #[test]
    fn a_sync_that_would_empty_a_populated_machine_is_refused() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("web"), profile("db")]);
        store
            .save(&SyncState {
                base: local.clone(),
                revision: RemoteRevision::default(),
                last_synced_at: "then".to_owned(),
            })
            .expect("seed the ancestor");
        let mut backend = Fake {
            remote: Some(SyncDocument::new(
                ProfileCatalog::default(),
                "other".into(),
                "now".into(),
            )),
            revision: 1,
            ..Fake::default()
        };

        let error = sync(
            &mut backend,
            &store,
            &local,
            &Extras::default(),
            "willpc",
            "now",
        )
        .expect_err("must refuse");

        assert!(matches!(error, SyncError::Remote { .. }));
        assert_eq!(backend.pushes, 0, "nothing may be written on the way out");
        assert_eq!(store.load().base.profiles.len(), 2, "ancestor stands still");
    }

    /// Settings and the sealed credential blob ride along whole.
    #[test]
    fn extras_travel_with_the_document() {
        let (store, _dir) = state_store();
        let local = catalog(vec![profile("web")]);
        let mut backend = Fake::default();
        let extras = Extras {
            settings: None,
            credentials: Some("deadbeef".to_owned()),
        };

        sync(&mut backend, &store, &local, &extras, "willpc", "now").expect("sync");
        assert_eq!(
            backend
                .remote
                .as_ref()
                .expect("pushed")
                .credentials
                .as_deref(),
            Some("deadbeef")
        );
    }

    /// A temp directory that cleans itself up, so the tests touch no shared
    /// state and can run in parallel.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU64, Ordering};
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
                let pid = std::process::id();
                let path = std::env::temp_dir().join(format!("adit-sync-{pid}-{unique}"));
                std::fs::create_dir_all(&path).expect("temp dir");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}

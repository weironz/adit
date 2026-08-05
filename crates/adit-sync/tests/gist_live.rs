//! End-to-end check of the Gist backend against the real GitHub API.
//!
//! `#[ignore]`d and env-driven: it needs a token and it creates a gist on the
//! account that owns it, so it never runs as part of `cargo test`.
//!
//! ```text
//! GITHUB_TOKEN=$(gh auth token) \
//!   cargo test -p adit-sync --test gist_live -- --ignored --nocapture
//! ```
//!
//! It exercises the sequence the unit tests deliberately cannot: a first push
//! that *creates* a gist, the id coming back through `assigned_id`, a second
//! machine's edits merging with the first, and the id being reused rather than
//! minting a second gist. Those are the parts where the real service's
//! behaviour — not our model of it — decides whether sync works.

use adit_domain::ConnectionProfile;
use adit_storage::ProfileCatalog;
use adit_sync::backend::gist::{GistBackend, GistConfig};
use adit_sync::orchestrate::{sync, Extras, SyncStateStore};
use adit_sync::SyncBackend;

fn profile(name: &str) -> ConnectionProfile {
    ConnectionProfile::new(name, format!("{name}.example"), 22, "will")
}

/// A scratch directory that cleans up after itself, so two runs cannot share
/// an ancestor file and quietly test nothing.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("adit-gist-live-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "needs GITHUB_TOKEN and creates a gist; see the module docs"]
fn a_real_gist_round_trips_through_two_machines() {
    let token = std::env::var("GITHUB_TOKEN").expect("GITHUB_TOKEN");
    assert!(!token.trim().is_empty(), "GITHUB_TOKEN is empty");

    // ---- machine A: first sync, with no gist yet ----
    let scratch_a = Scratch::new("a");
    let store_a = SyncStateStore::new(scratch_a.0.join("sync-state.json"));
    let mut backend_a = GistBackend::new(GistConfig {
        token: token.clone(),
        gist_id: None,
    });
    let local_a = ProfileCatalog::new(vec!["prod".into()], vec![profile("from-a")]);

    let outcome = sync(
        &mut backend_a,
        &store_a,
        &local_a,
        &Extras::default(),
        "machine-a",
        &adit_sync::rfc3339(std::time::SystemTime::now()),
    )
    .expect("first sync");
    assert!(outcome.uploaded, "a first sync must upload");

    let gist_id = backend_a
        .assigned_id()
        .expect("the first push must report the id GitHub minted");
    println!("created gist: https://gist.github.com/{gist_id}");

    // ---- machine B: fresh ancestor, its own session, same gist ----
    let scratch_b = Scratch::new("b");
    let store_b = SyncStateStore::new(scratch_b.0.join("sync-state.json"));
    let mut backend_b = GistBackend::new(GistConfig {
        token: token.clone(),
        gist_id: Some(gist_id.clone()),
    });
    let local_b = ProfileCatalog::new(vec!["staging".into()], vec![profile("from-b")]);

    let outcome_b = sync(
        &mut backend_b,
        &store_b,
        &local_b,
        &Extras::default(),
        "machine-b",
        &adit_sync::rfc3339(std::time::SystemTime::now()),
    )
    .expect("second machine sync");

    let names: Vec<&str> = outcome_b
        .catalog
        .profiles
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        names.contains(&"from-a") && names.contains(&"from-b"),
        "both machines' sessions must survive the merge, got {names:?}"
    );
    assert!(
        outcome_b.catalog.groups.contains(&"prod".to_owned())
            && outcome_b.catalog.groups.contains(&"staging".to_owned()),
        "groups from both sides must survive, got {:?}",
        outcome_b.catalog.groups
    );
    assert!(outcome_b.conflicts.is_empty(), "additions are not conflicts");

    // ---- machine A again: it must now see B's session ----
    let outcome_a2 = sync(
        &mut backend_a,
        &store_a,
        &local_a,
        &Extras::default(),
        "machine-a",
        &adit_sync::rfc3339(std::time::SystemTime::now()),
    )
    .expect("third sync");
    let names: Vec<&str> = outcome_a2
        .catalog
        .profiles
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        names.contains(&"from-b"),
        "machine A must pick up B's session, got {names:?}"
    );

    // The id is reused, never re-minted: a second gist here would mean every
    // sync scatters sessions into a new one.
    assert_eq!(
        backend_a.assigned_id().as_deref(),
        Some(gist_id.as_str()),
        "the gist id must be reused"
    );

    println!("verified: two machines merged, id reused ({gist_id})");
}

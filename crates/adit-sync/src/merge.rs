//! Three-way merge of two diverged profile catalogs.
//!
//! Sync is per-session, not per-file: two machines that each added a host must
//! end up with both, and only a host edited on *both* sides is a conflict. That
//! needs a common ancestor — the snapshot as it stood at the last successful
//! sync — because without it "changed" and "added" are indistinguishable, and
//! a delete on one side is indistinguishable from an add on the other. The
//! ancestor is what makes the rules below decidable; timestamps are not needed
//! and are not trusted (clocks on two machines disagree, and a restored backup
//! carries an old one).
//!
//! Profiles are keyed by their UUID, which never changes once minted.
//! Comparison is on canonical JSON rather than `PartialEq`, so the domain type
//! needs no extra derives and a field added later is covered automatically.

use std::collections::{HashMap, HashSet};

use adit_domain::{ConnectionProfile, ProfileId};
use adit_storage::ProfileCatalog;

/// Why a profile could not be merged automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Edited differently on both sides. The local edit is kept.
    BothEdited,
    /// Edited on one side, deleted on the other. The edit is kept: a deletion
    /// that loses someone's work is the worse of the two mistakes, and the
    /// user can always delete again.
    EditedAndDeleted,
    /// The same id appeared on both sides with different content without
    /// existing in the ancestor. UUIDs make this essentially impossible except
    /// through a restored backup or a hand-edited file; the local copy wins.
    BothAdded,
}

/// One profile the merge could not settle on its own.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub id: ProfileId,
    /// Name as shown to the user, taken from whichever side was kept.
    pub name: String,
    pub kind: ConflictKind,
    /// The side that lost, kept so the UI can offer it and nothing is ever
    /// silently discarded.
    pub discarded: Option<ConnectionProfile>,
}

/// What the merge did, for the sync status panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub added_from_remote: usize,
    pub added_locally: usize,
    pub updated_from_remote: usize,
    pub deleted: usize,
}

#[derive(Debug, Clone)]
pub struct MergeOutcome {
    pub catalog: ProfileCatalog,
    pub conflicts: Vec<Conflict>,
    pub stats: MergeStats,
}

impl MergeOutcome {
    /// Whether the merged result differs from what this machine already had.
    #[must_use]
    pub fn changed_locally(&self, local: &ProfileCatalog) -> bool {
        canonical_catalog(&self.catalog) != canonical_catalog(local)
    }
}

fn canonical(profile: &ConnectionProfile) -> String {
    // Deterministic for a fixed field order, which serde derives preserve.
    serde_json::to_string(profile).unwrap_or_default()
}

fn canonical_catalog(catalog: &ProfileCatalog) -> String {
    serde_json::to_string(catalog).unwrap_or_default()
}

fn index(profiles: &[ConnectionProfile]) -> HashMap<ProfileId, &ConnectionProfile> {
    profiles.iter().map(|profile| (profile.id, profile)).collect()
}

/// Merge `local` and `remote`, using `base` as the common ancestor.
///
/// `base` is the catalog as it was at the end of the last successful sync. On
/// the very first sync there is none: pass an empty catalog and every profile
/// on either side reads as an addition, which is the correct behaviour — two
/// machines meeting for the first time should end up with the union.
#[must_use]
pub fn three_way(
    base: &ProfileCatalog,
    local: &ProfileCatalog,
    remote: &ProfileCatalog,
) -> MergeOutcome {
    let (base_ix, local_ix, remote_ix) = (
        index(&base.profiles),
        index(&local.profiles),
        index(&remote.profiles),
    );

    let mut kept: HashMap<ProfileId, ConnectionProfile> = HashMap::new();
    let mut conflicts = Vec::new();
    let mut stats = MergeStats::default();

    let ids: HashSet<ProfileId> = base_ix
        .keys()
        .chain(local_ix.keys())
        .chain(remote_ix.keys())
        .copied()
        .collect();

    for id in ids {
        let (b, l, r) = (base_ix.get(&id), local_ix.get(&id), remote_ix.get(&id));
        match (b, l, r) {
            // Gone from both sides, or never anywhere.
            (_, None, None) => {
                if b.is_some() {
                    stats.deleted += 1;
                }
            }
            // Added on exactly one side.
            (None, Some(l), None) => {
                stats.added_locally += 1;
                kept.insert(id, (*l).clone());
            }
            (None, None, Some(r)) => {
                stats.added_from_remote += 1;
                kept.insert(id, (*r).clone());
            }
            // Added on both sides — only reachable via a restored backup.
            (None, Some(l), Some(r)) => {
                if canonical(l) != canonical(r) {
                    conflicts.push(Conflict {
                        id,
                        name: l.name.clone(),
                        kind: ConflictKind::BothAdded,
                        discarded: Some((*r).clone()),
                    });
                }
                kept.insert(id, (*l).clone());
            }
            // Deleted on one side; keep it only if the other side edited it.
            (Some(b), Some(l), None) => {
                if canonical(l) == canonical(b) {
                    stats.deleted += 1;
                } else {
                    conflicts.push(Conflict {
                        id,
                        name: l.name.clone(),
                        kind: ConflictKind::EditedAndDeleted,
                        discarded: None,
                    });
                    kept.insert(id, (*l).clone());
                }
            }
            (Some(b), None, Some(r)) => {
                if canonical(r) == canonical(b) {
                    stats.deleted += 1;
                } else {
                    conflicts.push(Conflict {
                        id,
                        name: r.name.clone(),
                        kind: ConflictKind::EditedAndDeleted,
                        discarded: None,
                    });
                    stats.added_from_remote += 1;
                    kept.insert(id, (*r).clone());
                }
            }
            // Present everywhere: whoever moved away from the ancestor wins.
            (Some(b), Some(l), Some(r)) => {
                let (cb, cl, cr) = (canonical(b), canonical(l), canonical(r));
                if cl == cr {
                    kept.insert(id, (*l).clone());
                } else if cl == cb {
                    stats.updated_from_remote += 1;
                    kept.insert(id, (*r).clone());
                } else if cr == cb {
                    kept.insert(id, (*l).clone());
                } else {
                    conflicts.push(Conflict {
                        id,
                        name: l.name.clone(),
                        kind: ConflictKind::BothEdited,
                        discarded: Some((*r).clone()),
                    });
                    kept.insert(id, (*l).clone());
                }
            }
        }
    }

    // Order: this machine's arrangement first, then anything only the remote
    // had, appended in its own order. Sorting by `sort_order` instead would
    // look tidier and would silently rewrite a drag-and-drop ordering the user
    // set by hand.
    let mut profiles = Vec::with_capacity(kept.len());
    let mut placed: HashSet<ProfileId> = HashSet::new();
    for profile in &local.profiles {
        if let Some(merged) = kept.get(&profile.id) {
            profiles.push(merged.clone());
            placed.insert(profile.id);
        }
    }
    for profile in &remote.profiles {
        if placed.contains(&profile.id) {
            continue;
        }
        if let Some(merged) = kept.get(&profile.id) {
            profiles.push(merged.clone());
            placed.insert(profile.id);
        }
    }

    // A `HashSet` gives no iteration order, so sort what the user will read.
    conflicts.sort_by(|a, b| a.name.cmp(&b.name));

    MergeOutcome {
        catalog: {
            let groups = merge_groups(base, local, remote);
            // Icons have to be carried across explicitly. `ProfileCatalog::new`
            // starts them empty, and leaving it here would have quietly wiped
            // every group icon on every sync — a loss that reads as "the
            // feature never worked" rather than as a merge bug.
            let icons = merge_group_icons(local, remote);
            ProfileCatalog::with_group_icons(groups, icons, profiles)
        },
        conflicts,
        stats,
    }
}

/// Groups are a flat name list, so the same ancestor rules apply per name:
/// a name absent from a side that had it in the ancestor was deleted there.
/// Which icon each group ends up with.
///
/// Local wins where both sides chose, matching the rule for a session edited on
/// both machines: this machine's choice is the one in front of the user.
/// `with_group_icons` prunes whatever no longer names a live group.
fn merge_group_icons(
    local: &ProfileCatalog,
    remote: &ProfileCatalog,
) -> std::collections::BTreeMap<String, String> {
    let mut icons = remote.group_icons.clone();
    icons.extend(
        local
            .group_icons
            .iter()
            .map(|(name, icon)| (name.clone(), icon.clone())),
    );
    icons
}

fn merge_groups(
    base: &ProfileCatalog,
    local: &ProfileCatalog,
    remote: &ProfileCatalog,
) -> Vec<String> {
    let base_set: HashSet<&String> = base.groups.iter().collect();
    let local_set: HashSet<&String> = local.groups.iter().collect();
    let remote_set: HashSet<&String> = remote.groups.iter().collect();

    let keep = |name: &String| {
        let in_base = base_set.contains(name);
        let in_local = local_set.contains(name);
        let in_remote = remote_set.contains(name);
        if in_base {
            // Survived unless a side deleted it.
            in_local && in_remote
        } else {
            // Added somewhere.
            in_local || in_remote
        }
    };

    let mut groups = Vec::new();
    for name in local.groups.iter().chain(remote.groups.iter()) {
        if keep(name) && !groups.contains(name) {
            groups.push(name.clone());
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile::new(name, format!("{name}.example"), 22, "will")
    }

    fn catalog(profiles: Vec<ConnectionProfile>) -> ProfileCatalog {
        ProfileCatalog::new(Vec::new(), profiles)
    }

    /// The whole point of per-session merge: each machine's new host survives.
    #[test]
    fn additions_from_both_sides_are_kept() {
        let base = catalog(vec![]);
        let mine = profile("mine");
        let theirs = profile("theirs");
        let out = three_way(
            &base,
            &catalog(vec![mine.clone()]),
            &catalog(vec![theirs.clone()]),
        );
        let names: Vec<_> = out.catalog.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["mine", "theirs"]);
        assert!(out.conflicts.is_empty());
    }

    /// An edit on one side only is taken, with no conflict.
    #[test]
    fn a_one_sided_edit_wins_without_a_conflict() {
        let original = profile("web");
        let base = catalog(vec![original.clone()]);
        let mut edited = original.clone();
        edited.host = "web-2.example".to_owned();

        let from_remote = three_way(&base, &base, &catalog(vec![edited.clone()]));
        assert_eq!(from_remote.catalog.profiles[0].host, "web-2.example");
        assert_eq!(from_remote.stats.updated_from_remote, 1);
        assert!(from_remote.conflicts.is_empty());

        let from_local = three_way(&base, &catalog(vec![edited]), &base);
        assert_eq!(from_local.catalog.profiles[0].host, "web-2.example");
        assert!(from_local.conflicts.is_empty());
    }

    /// A delete propagates only when the other side left the profile alone —
    /// otherwise the deletion would throw away an edit made elsewhere.
    #[test]
    fn a_delete_propagates_but_never_over_an_edit() {
        let original = profile("db");
        let base = catalog(vec![original.clone()]);

        let clean = three_way(&base, &base, &catalog(vec![]));
        assert!(clean.catalog.profiles.is_empty());
        assert_eq!(clean.stats.deleted, 1);

        let mut edited = original.clone();
        edited.username = "root".to_owned();
        let contested = three_way(&base, &catalog(vec![edited]), &catalog(vec![]));
        assert_eq!(contested.catalog.profiles.len(), 1);
        assert_eq!(contested.conflicts[0].kind, ConflictKind::EditedAndDeleted);
    }

    /// Both sides edited the same profile: local is kept, the remote version is
    /// handed back rather than dropped.
    #[test]
    fn a_double_edit_conflicts_and_keeps_the_loser() {
        let original = profile("api");
        let base = catalog(vec![original.clone()]);
        let mut mine = original.clone();
        mine.port = 2222;
        let mut theirs = original.clone();
        theirs.port = 2022;

        let out = three_way(&base, &catalog(vec![mine]), &catalog(vec![theirs]));
        assert_eq!(out.catalog.profiles[0].port, 2222);
        assert_eq!(out.conflicts.len(), 1);
        assert_eq!(out.conflicts[0].kind, ConflictKind::BothEdited);
        assert_eq!(out.conflicts[0].discarded.as_ref().expect("kept").port, 2022);
    }

    /// Identical edits on both machines are not a conflict.
    #[test]
    fn the_same_edit_on_both_sides_is_not_a_conflict() {
        let original = profile("cache");
        let base = catalog(vec![original.clone()]);
        let mut edited = original;
        edited.port = 6379;
        let out = three_way(&base, &catalog(vec![edited.clone()]), &catalog(vec![edited]));
        assert_eq!(out.catalog.profiles[0].port, 6379);
        assert!(out.conflicts.is_empty());
    }

    /// With no ancestor (first ever sync) the two catalogs are unioned rather
    /// than one overwriting the other.
    #[test]
    fn a_first_sync_unions_both_machines() {
        let mine = profile("a");
        let theirs = profile("b");
        let out = three_way(&catalog(vec![]), &catalog(vec![mine]), &catalog(vec![theirs]));
        assert_eq!(out.catalog.profiles.len(), 2);
        assert!(out.conflicts.is_empty());
    }

    /// The local drag-and-drop ordering is preserved; remote-only entries are
    /// appended instead of being interleaved by some sort key.
    #[test]
    fn local_ordering_survives_and_remote_extras_append() {
        let (a, b, c) = (profile("a"), profile("b"), profile("c"));
        let out = three_way(
            &catalog(vec![]),
            &catalog(vec![b, a]),
            &catalog(vec![c]),
        );
        let names: Vec<_> = out.catalog.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["b", "a", "c"]);
    }

    /// A group deleted on one side goes, one added on either side stays.
    #[test]
    fn groups_follow_the_same_ancestor_rules() {
        let base = ProfileCatalog::new(vec!["keep".into(), "drop".into()], vec![]);
        let local = ProfileCatalog::new(vec!["keep".into(), "drop".into(), "mine".into()], vec![]);
        let remote = ProfileCatalog::new(vec!["keep".into(), "theirs".into()], vec![]);
        let out = three_way(&base, &local, &remote);
        assert_eq!(out.catalog.groups, ["keep", "mine", "theirs"]);
    }
}

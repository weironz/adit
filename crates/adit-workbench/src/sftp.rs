//! Interaction state shared by frontends displaying a two-pane SFTP browser.
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpPane {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpSortKey {
    Name,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy)]
pub struct SortOrder {
    pub key: SftpSortKey,
    pub ascending: bool,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self {
            key: SftpSortKey::Name,
            ascending: true,
        }
    }
}

impl SortOrder {
    /// Directories stay first even when the column order is descending.
    pub fn compare(self, a: ListingEntry<'_>, b: ListingEntry<'_>) -> Ordering {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let order = match self.key {
                SftpSortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SftpSortKey::Size => a.size.cmp(&b.size),
                SftpSortKey::Modified => a.mtime.unwrap_or(0).cmp(&b.mtime.unwrap_or(0)),
            };
            if self.ascending {
                order
            } else {
                order.reverse()
            }
        })
    }
}

#[derive(Clone, Copy)]
pub struct ListingEntry<'a> {
    pub name: &'a str,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<u64>,
}

#[derive(Default)]
pub struct PaneState {
    pub selected: BTreeSet<String>,
    pub sort: SortOrder,
}

impl PaneState {
    pub fn sort_by(&mut self, key: SftpSortKey) {
        self.sort = SortOrder {
            key,
            ascending: self.sort.key != key || !self.sort.ascending,
        };
    }

    pub fn drag_count(&self, name: &str) -> usize {
        if self.selected.contains(name) {
            self.selected.len()
        } else {
            1
        }
    }

    pub fn dragged_names(&self, name: String) -> Vec<String> {
        if self.drag_count(&name) > 1 {
            self.selected.iter().cloned().collect()
        } else {
            vec![name]
        }
    }
}

#[derive(Default)]
pub struct SftpPanes {
    local: PaneState,
    remote: PaneState,
    last_click: Option<(SftpPane, String, Instant)>,
}

#[derive(PartialEq, Eq, Debug)]
pub enum RowAction {
    SelectionChanged,
    TransferFile,
}

impl SftpPanes {
    pub fn pane(&self, pane: SftpPane) -> &PaneState {
        match pane {
            SftpPane::Local => &self.local,
            SftpPane::Remote => &self.remote,
        }
    }

    pub fn pane_mut(&mut self, pane: SftpPane) -> &mut PaneState {
        match pane {
            SftpPane::Local => &mut self.local,
            SftpPane::Remote => &mut self.remote,
        }
    }

    pub fn press(&mut self, pane: SftpPane, name: &str, now: Instant) -> RowAction {
        if matches!(&self.last_click, Some((p, n, t)) if *p == pane && n == name && now.duration_since(*t) < Duration::from_millis(450))
        {
            self.last_click = None;
            return RowAction::TransferFile;
        }
        self.last_click = Some((pane, name.to_owned(), now));
        let selected = &mut self.pane_mut(pane).selected;
        if !selected.remove(name) {
            selected.insert(name.to_owned());
        }
        RowAction::SelectionChanged
    }

    /// Reopening the browser preserves column order, but starts a new selection.
    pub fn close(&mut self) {
        self.local.selected.clear();
        self.remote.selected.clear();
        self.last_click = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clicks_and_drag_batches_are_scoped_to_their_pane() {
        use SftpPane::{Local, Remote};
        let now = Instant::now();
        let mut panes = SftpPanes::default();
        assert_eq!(panes.press(Local, "b", now), RowAction::SelectionChanged);
        assert_eq!(panes.press(Local, "a", now), RowAction::SelectionChanged);
        assert_eq!(panes.press(Local, "a", now), RowAction::TransferFile);
        assert_eq!(panes.pane(Local).dragged_names("a".into()), ["a", "b"]);
        assert_eq!(panes.pane(Local).dragged_names("c".into()), ["c"]);
        assert_eq!(panes.press(Remote, "a", now), RowAction::SelectionChanged);
        assert_eq!(panes.press(Local, "a", now), RowAction::SelectionChanged);
        panes.pane_mut(Remote).sort_by(SftpSortKey::Size);
        panes.close();
        assert_eq!(panes.pane(Remote).sort.key, SftpSortKey::Size);
        assert!(panes.pane(Remote).selected.is_empty());
        assert_eq!(panes.press(Local, "a", now), RowAction::SelectionChanged);
    }

    #[test]
    fn sorting_keeps_directories_first_in_both_directions() {
        let mut pane = PaneState::default();
        let a = ListingEntry {
            name: "alpha",
            is_dir: false,
            size: 10,
            mtime: Some(100),
        };
        let b = ListingEntry {
            name: "beta",
            is_dir: false,
            size: 5,
            mtime: Some(200),
        };
        assert_eq!(pane.sort.compare(a, b), Ordering::Less);
        pane.sort_by(SftpSortKey::Name);
        assert_eq!(pane.sort.compare(a, b), Ordering::Greater);
        assert_eq!(
            pane.sort.compare(ListingEntry { is_dir: true, ..a }, b),
            Ordering::Less
        );
        pane.sort_by(SftpSortKey::Size);
        assert_eq!(pane.sort.compare(a, b), Ordering::Greater);
        pane.sort_by(SftpSortKey::Modified);
        assert_eq!(pane.sort.compare(a, b), Ordering::Less);
    }
}

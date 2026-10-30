//! Suppress only text representations of file URIs that Adit itself published.
//! Keep a bounded history so a delayed read cannot echo an older selection.
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct RemoteFileEcho {
    texts: VecDeque<String>,
    file_formats_present: bool,
}

impl RemoteFileEcho {
    pub(super) fn remember(&mut self, uris: &[String]) {
        let paths: Vec<&str> = uris
            .iter()
            .map(|uri| uri.strip_prefix("file://").unwrap_or(uri))
            .collect();
        for text in [
            paths.join("\n"),
            paths.join("\r\n"),
            uris.join("\n"),
            uris.join("\r\n"),
        ] {
            if !text.is_empty() && !self.texts.contains(&text) {
                self.texts.push_back(text);
            }
        }
        while self.texts.len() > 32 {
            self.texts.pop_front();
        }
    }

    pub(super) fn contains(&self, text: &str) -> bool {
        self.texts.iter().any(|own| own == text)
    }

    /// An unreadable or indeterminate X11 target list cannot prove that the
    /// clipboard stopped holding one of our file selections.
    pub(super) fn may_be_file_formats(formats: Option<&[String]>) -> bool {
        let Some(formats) = formats else {
            return true;
        };
        formats.is_empty()
            || formats.iter().any(|format| {
                matches!(
                    format.as_str(),
                    "Unknown"
                        | "text/uri-list"
                        | "x-special/gnome-copied-files"
                        | "x-special/nautilus-clipboard"
                )
            })
    }

    pub(super) fn set_file_formats_present(&mut self, present: bool) {
        self.file_formats_present = present;
    }

    pub(super) fn should_suppress(&self, text: &str) -> bool {
        self.file_formats_present && self.contains(text)
    }

    pub(super) fn is_stale_or_own_echo(
        &self,
        text: &str,
        token: Option<u32>,
        current: u32,
    ) -> bool {
        token.is_some_and(|seen| seen != current) || self.should_suppress(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_only_our_published_x11_text_including_delayed_reads() {
        let mut echo = RemoteFileEcho::default();
        echo.remember(&[
            "file:///tmp/adit-fixture/a%20b.txt".into(),
            "file:///tmp/adit-fixture/c.txt".into(),
        ]);
        let old = "/tmp/adit-fixture/a%20b.txt\n/tmp/adit-fixture/c.txt";
        assert!(echo.contains(old));
        assert!(echo.contains("/tmp/adit-fixture/a%20b.txt\r\n/tmp/adit-fixture/c.txt"));
        assert!(echo.contains("file:///tmp/adit-fixture/a%20b.txt\nfile:///tmp/adit-fixture/c.txt"));
        echo.remember(&["file:///tmp/adit-next/other.txt".into()]);
        assert!(echo.contains(old));
        // A later external plain-text copy of the same path remains text.
        echo.set_file_formats_present(true);
        assert!(echo.should_suppress(old));
        echo.set_file_formats_present(false);
        assert!(!echo.should_suppress(old));
        assert!(!echo.is_stale_or_own_echo(old, Some(4), 4));
        assert!(echo.is_stale_or_own_echo(old, Some(4), 5));
        assert!(!echo.contains("ordinary Chinese text 中文"));
        assert!(!echo.contains("file:///home/user/external.txt"));
        assert!(RemoteFileEcho::may_be_file_formats(None));
        assert!(RemoteFileEcho::may_be_file_formats(Some(&[])));
        assert!(RemoteFileEcho::may_be_file_formats(Some(&[
            "Unknown".into()
        ])));
        assert!(RemoteFileEcho::may_be_file_formats(Some(&[
            "UTF8_STRING".into(),
            "text/uri-list".into()
        ])));
        assert!(!RemoteFileEcho::may_be_file_formats(Some(&[
            "UTF8_STRING".into()
        ])));
    }
}

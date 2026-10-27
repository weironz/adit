use std::time::{Duration, Instant};

use adit_domain::{OutputTrigger, TriggerAction};
use regex::bytes::{Regex, RegexBuilder};

use super::AnsiStripper;

const BUFFER_LIMIT: usize = 8192;
const ACTION_LIMIT: usize = 16;
const COOLDOWN: Duration = Duration::from_secs(1);

struct Rule {
    regex: Regex,
    action: TriggerAction,
    last_start: Option<usize>,
    last_fired: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct TriggerEngine {
    rules: Vec<Rule>,
    stripper: AnsiStripper,
    bytes: Vec<u8>,
    base: usize,
}

impl TriggerEngine {
    pub(crate) fn new(config: &[OutputTrigger]) -> Result<Self, String> {
        if config.len() > 16 || config.iter().any(|rule| rule.pattern.len() > 512) {
            return Err("触发器数量或匹配表达式超过限制".into());
        }
        let mut rules = Vec::new();
        for trigger in config.iter().filter(|rule| rule.enabled) {
            let regex = RegexBuilder::new(&trigger.pattern)
                .size_limit(256 * 1024)
                .build()
                .map_err(|error| format!("触发器正则表达式无效：{error}"))?;
            if regex.is_match(b"") {
                return Err("触发器表达式不能匹配空字符串".into());
            }
            rules.push(Rule {
                regex,
                action: trigger.action.clone(),
                last_start: None,
                last_fired: None,
            });
        }
        Ok(Self {
            rules,
            ..Default::default()
        })
    }

    pub(crate) fn feed(&mut self, chunk: &[u8], now: Instant) -> Vec<TriggerAction> {
        if self.rules.is_empty() {
            return Vec::new();
        }
        let mut clean = Vec::with_capacity(chunk.len().min(BUFFER_LIMIT));
        self.stripper.strip(chunk, &mut clean);
        self.bytes.extend_from_slice(&clean);
        if self.bytes.len() > BUFFER_LIMIT {
            let remove = self.bytes.len() - BUFFER_LIMIT;
            self.bytes.drain(..remove);
            self.base = self.base.saturating_add(remove);
        }
        let mut actions = Vec::new();
        for rule in &mut self.rules {
            for found in rule.regex.find_iter(&self.bytes) {
                if found.is_empty() {
                    continue;
                }
                let start = self.base.saturating_add(found.start());
                if rule.last_start.is_some_and(|last| start <= last) {
                    continue;
                }
                rule.last_start = Some(start);
                if rule
                    .last_fired
                    .is_none_or(|last| now.saturating_duration_since(last) >= COOLDOWN)
                    && actions.len() < ACTION_LIMIT
                {
                    rule.last_fired = Some(now);
                    actions.push(rule.action.clone());
                }
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify(pattern: &str) -> OutputTrigger {
        OutputTrigger {
            enabled: true,
            pattern: pattern.into(),
            action: TriggerAction::Notify,
        }
    }

    #[test]
    fn split_unicode_and_ansi_match_only_once() {
        let mut engine = TriggerEngine::new(&[notify("错误")]).unwrap();
        let now = Instant::now();
        let bytes = "错\u{1b}[31m误".as_bytes();
        for byte in &bytes[..bytes.len() - 1] {
            assert!(engine.feed(&[*byte], now).is_empty());
        }
        assert_eq!(
            engine.feed(&bytes[bytes.len() - 1..], now),
            vec![TriggerAction::Notify]
        );
        assert!(engine
            .feed(b" more output", now + Duration::from_secs(2))
            .is_empty());
    }

    #[test]
    fn controls_do_not_cause_actions_and_repeated_output_is_rate_limited() {
        let mut engine = TriggerEngine::new(&[notify("danger")]).unwrap();
        let now = Instant::now();
        for byte in b"\x1bPq danger\x1b\\\x1b]title\x1bXdangerdanger\x1b\\" {
            assert!(engine.feed(&[*byte], now).is_empty());
        }
        assert_eq!(engine.feed(b"danger", now), vec![TriggerAction::Notify]);
        assert!(engine.feed(b" danger", now).is_empty());
        assert_eq!(
            engine.feed(b" danger", now + Duration::from_secs(2)),
            vec![TriggerAction::Notify]
        );
    }

    #[test]
    fn invalid_patterns_and_empty_matches_are_rejected() {
        assert!(TriggerEngine::new(&[notify("[")]).is_err());
        assert!(TriggerEngine::new(&[notify("a*")]).is_err());
    }

    #[test]
    fn retained_output_is_bounded_and_sessions_do_not_share_match_state() {
        let mut first = TriggerEngine::new(&[notify("ready")]).unwrap();
        let mut second = TriggerEngine::new(&[notify("ready")]).unwrap();
        let now = Instant::now();
        first.feed(&vec![b'x'; BUFFER_LIMIT * 4], now);
        assert_eq!(first.bytes.len(), BUFFER_LIMIT);
        assert_eq!(first.feed(b"ready", now), vec![TriggerAction::Notify]);
        assert_eq!(second.feed(b"ready", now), vec![TriggerAction::Notify]);
    }
}

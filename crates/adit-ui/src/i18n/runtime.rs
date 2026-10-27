//! Adapter for the existing string diagnostics emitted by the engine. Keep
//! captures opaque: a profile/file name can itself be a translated UI word.
use super::{fill, translate, COMMON, RUNTIME};
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

struct Rule {
    matcher: Regex,
    english: &'static str,
    error_slots: Vec<usize>,
    specificity: usize,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let holes = Regex::new(r"\{(?:[A-Za-z_][A-Za-z_0-9]*)?\}").expect("template slots");
        let mut rules = Vec::new();
        for &(source, english) in RUNTIME.iter().chain(COMMON) {
            if !holes.is_match(source) {
                continue;
            }
            let mut pattern = String::from("(?s)^");
            let mut end = 0;
            let mut error_slots = Vec::new();
            let mut specificity = 0;
            for (index, hole) in holes.find_iter(source).enumerate() {
                specificity += hole.start() - end;
                pattern.push_str(&regex::escape(&source[end..hole.start()]));
                pattern.push_str("(.*?)");
                if hole.as_str() == "{error}" {
                    error_slots.push(index);
                }
                end = hole.end();
            }
            // No fully generic template may rewrite arbitrary data.
            specificity += source.len() - end;
            if specificity == 0 {
                continue;
            }
            pattern.push_str(&regex::escape(&source[end..]));
            pattern.push('$');
            rules.push(Rule {
                matcher: Regex::new(&pattern).expect("escaped diagnostic template"),
                english,
                error_slots,
                specificity,
            });
        }
        // A specific diagnostic must win over wrappers such as "Line {}: {}".
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.specificity));
        rules
    })
}

pub(super) fn render(english: bool, source: &str) -> String {
    render_inner(english, source, 0)
}

fn render_inner(english: bool, source: &str, depth: usize) -> String {
    if !english || depth >= 8 {
        return source.to_owned();
    }
    let exact = translate(true, "", source);
    if exact != source {
        return exact.to_owned();
    }
    static EXACT: OnceLock<HashMap<&str, &str>> = OnceLock::new();
    if let Some(english) = EXACT
        .get_or_init(|| {
            RUNTIME
                .iter()
                .copied()
                .filter(|(source, _)| !source.contains('{'))
                .collect()
        })
        .get(source)
    {
        return (*english).to_owned();
    }
    // Fast path for the overwhelmingly common case of already-English engine
    // diagnostics. Chinese payloads still pass the anchored template check.
    if !source
        .chars()
        .any(|c| ('\u{3400}'..='\u{9fff}').contains(&c))
    {
        return source.to_owned();
    }
    for rule in rules() {
        if let Some(captures) = rule.matcher.captures(source) {
            let values: Vec<String> = captures
                .iter()
                .skip(1)
                .enumerate()
                .map(|(i, value)| {
                    let value = value.map_or("", |value| value.as_str());
                    if rule.error_slots.contains(&i) {
                        render_inner(true, value, depth + 1)
                    } else {
                        value.to_owned()
                    }
                })
                .collect();
            let args: Vec<&dyn std::fmt::Display> =
                values.iter().map(|v| v as &dyn std::fmt::Display).collect();
            return fill(rule.english, &args);
        }
    }
    source.to_owned()
}

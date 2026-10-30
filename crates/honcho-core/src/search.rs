use std::ops::Range;

use regex::{Regex, RegexBuilder};
use ropey::Rope;

pub const MAX_MATCHES: usize = 100_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

impl SearchQuery {
    /// `Ok(None)` for an empty query.
    pub fn compile(&self) -> Result<Option<Regex>, String> {
        if self.text.is_empty() {
            return Ok(None);
        }
        let mut pattern = if self.regex {
            self.text.clone()
        } else {
            regex::escape(&self.text)
        };
        if self.whole_word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive)
            .multi_line(true)
            .build()
            .map(Some)
            .map_err(|e| match e {
                regex::Error::Syntax(s) => s
                    .lines()
                    .last()
                    .unwrap_or("invalid regex")
                    .trim()
                    .to_string(),
                other => other.to_string(),
            })
    }
}

/// Non-empty matches in char offsets.
pub fn find_all(rope: &Rope, re: &Regex) -> Vec<Range<usize>> {
    let text = rope.to_string();
    re.find_iter(&text)
        .filter(|m| !m.is_empty())
        .take(MAX_MATCHES)
        .map(|m| rope.byte_to_char(m.start())..rope.byte_to_char(m.end()))
        .collect()
}

fn unescape(replacement: &str) -> String {
    replacement.replace("\\n", "\n").replace("\\t", "\t")
}

/// Replacement for the match at `range`; in regex mode `$1`, `\n` and `\t` are expanded.
pub fn replacement_for(
    rope: &Rope,
    re: &Regex,
    is_regex: bool,
    range: &Range<usize>,
    replacement: &str,
) -> String {
    if !is_regex {
        return replacement.to_string();
    }
    let text = rope.to_string();
    let at = rope.char_to_byte(range.start);
    let replacement = unescape(replacement);
    match re.captures_at(&text, at) {
        Some(caps) if caps.get(0).is_some_and(|m| m.start() == at) => {
            let mut out = String::new();
            caps.expand(&replacement, &mut out);
            out
        }
        _ => replacement,
    }
}

/// Edits that replace every match.
pub fn replace_all(
    rope: &Rope,
    re: &Regex,
    is_regex: bool,
    replacement: &str,
) -> Vec<(Range<usize>, String)> {
    let text = rope.to_string();
    let replacement = if is_regex {
        unescape(replacement)
    } else {
        replacement.to_string()
    };
    re.captures_iter(&text)
        .filter_map(|caps| {
            let m = caps.get(0)?;
            if m.is_empty() {
                return None;
            }
            let with = if is_regex {
                let mut out = String::new();
                caps.expand(&replacement, &mut out);
                out
            } else {
                replacement.clone()
            };
            Some((
                rope.byte_to_char(m.start())..rope.byte_to_char(m.end()),
                with,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_search_is_case_insensitive_and_escaped() {
        let rope = Rope::from_str("SELECT a.* FROM t; select A.* from u;");
        let re = q("a.*").compile().unwrap().unwrap();
        assert_eq!(find_all(&rope, &re), vec![7..10, 26..29]);
    }

    #[test]
    fn case_and_whole_word() {
        let rope = Rope::from_str("id user_id ID");
        let re = SearchQuery {
            whole_word: true,
            ..q("id")
        }
        .compile()
        .unwrap()
        .unwrap();
        assert_eq!(find_all(&rope, &re), vec![0..2, 11..13]);
        let re = SearchQuery {
            whole_word: true,
            case_sensitive: true,
            ..q("id")
        }
        .compile()
        .unwrap()
        .unwrap();
        assert_eq!(find_all(&rope, &re), vec![0..2]);
    }

    #[test]
    fn regex_replace_with_groups() {
        let rope = Rope::from_str("ชื่อ = 'a', age = 'b'");
        let query = SearchQuery {
            regex: true,
            ..q(r"(\w+) = '(\w)'")
        };
        let re = query.compile().unwrap().unwrap();
        let edits = replace_all(&rope, &re, true, "$2 AS ${1}");
        let mut text = rope.clone();
        for (r, t) in edits.iter().rev() {
            text.remove(r.clone());
            text.insert(r.start, t);
        }
        assert_eq!(text.to_string(), "a AS ชื่อ, b AS age");
        let one = replacement_for(&rope, &re, true, &edits[1].0, "[$1]");
        assert_eq!(one, "[age]");
    }

    #[test]
    fn bad_regex_reports_error() {
        let err = SearchQuery {
            regex: true,
            ..q("(")
        }
        .compile()
        .unwrap_err();
        assert!(!err.is_empty());
    }
}

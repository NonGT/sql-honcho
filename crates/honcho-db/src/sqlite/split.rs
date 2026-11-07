//! Splits a script into statements on `;`, skipping strings, quoted names
//! and comments. Inside CREATE TRIGGER a `;` only ends the statement right
//! after the END that closes the body, not one closing a CASE.

use crate::script::Statement;

pub fn split(sql: &str) -> Vec<Statement<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut i = 0;
    // first byte of code in the current statement, and its line
    let mut start: Option<(usize, u32)> = None;
    // the statement's first words, to spot CREATE [TEMP] TRIGGER
    let mut words: Vec<&str> = Vec::new();
    // the last thing before a `;` was the END of a trigger body
    let mut after_end = false;
    let mut cases = 0usize;

    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b';' {
            if !is_trigger(&words) || after_end {
                if let Some((s, l)) = start.take() {
                    out.push(Statement {
                        text: sql[s..i].trim_end(),
                        start: s,
                        line: l,
                    });
                }
                words.clear();
                cases = 0;
            }
            after_end = false;
            i += 1;
            continue;
        }
        let end = match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = sql[i..].find('\n').map_or(b.len(), |p| i + p);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = sql[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
                line += sql[i..end].matches('\n').count() as u32;
                i = end;
                continue;
            }
            b'\'' | b'"' | b'`' | b'[' => {
                let close = if c == b'[' { b']' } else { c };
                let mut j = i + 1;
                loop {
                    match b[j..].iter().position(|&x| x == close) {
                        // a doubled quote is an escaped one; [] can't nest
                        Some(p) if close != b']' && b.get(j + p + 1) == Some(&close) => j += p + 2,
                        Some(p) => break j + p + 1,
                        None => break b.len(),
                    }
                }
            }
            _ if word_byte(c) => {
                let mut j = i;
                while j < b.len() && word_byte(b[j]) {
                    j += 1;
                }
                let word = &sql[i..j];
                if words.len() < 3 {
                    words.push(word);
                }
                start.get_or_insert((i, line));
                after_end = false;
                if word.eq_ignore_ascii_case("CASE") {
                    cases += 1;
                } else if word.eq_ignore_ascii_case("END") {
                    if cases == 0 {
                        after_end = true;
                    } else {
                        cases -= 1;
                    }
                }
                i = j;
                continue;
            }
            _ => i + 1,
        };
        start.get_or_insert((i, line));
        after_end = false;
        line += sql[i..end].matches('\n').count() as u32;
        i = end;
    }
    if let Some((s, l)) = start {
        let text = sql[s..].trim_end();
        if !text.is_empty() {
            out.push(Statement {
                text,
                start: s,
                line: l,
            });
        }
    }
    out
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_trigger(words: &[&str]) -> bool {
    let is = |i: usize, w: &str| words.get(i).is_some_and(|x| x.eq_ignore_ascii_case(w));
    is(0, "CREATE")
        && (is(1, "TRIGGER") || ((is(1, "TEMP") || is(1, "TEMPORARY")) && is(2, "TRIGGER")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        split(sql).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn splits_on_semicolons_past_quotes_and_comments() {
        assert_eq!(texts("SELECT 1; SELECT 2;"), ["SELECT 1", "SELECT 2"]);
        assert_eq!(
            texts("SELECT 'a;''b', \"c;d\", [e;f], `g;h` -- ;\n; SELECT /* ; */ 2"),
            [
                "SELECT 'a;''b', \"c;d\", [e;f], `g;h` -- ;",
                "SELECT /* ; */ 2"
            ]
        );
        assert_eq!(texts(" ; ;\n-- only a comment\n"), Vec::<&str>::new());
        assert_eq!(texts("SELECT 'abc; x"), ["SELECT 'abc; x"]);
    }

    #[test]
    fn keeps_trigger_bodies_whole() {
        let sql = "CREATE TEMP TRIGGER t AFTER INSERT ON a BEGIN\n  UPDATE b SET n = n + 1;\n  \
                   SELECT CASE WHEN 1 THEN 2 END;\nEND;\nSELECT 3;\ncreate trigger u before delete on a begin select 1; end";
        let s = split(sql);
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s[0].text.ends_with("\nEND"), "{:?}", s[0].text);
        assert_eq!(s[1].text, "SELECT 3");
        assert_eq!(s.iter().map(|s| s.line).collect::<Vec<_>>(), [1, 5, 6]);
        for st in &s {
            assert_eq!(&sql[st.start..st.start + st.text.len()], st.text);
        }
        // END as a column name doesn't end anything outside a trigger
        assert_eq!(texts("SELECT 1 AS x; SELECT 2").len(), 2);
    }
}

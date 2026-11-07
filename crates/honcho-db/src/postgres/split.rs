//! Splits a script into statements on `;`, skipping strings, quoted names,
//! comments and dollar-quoted bodies, the way psql does.

use crate::script::Statement;

pub fn split(sql: &str) -> Vec<Statement<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut i = 0;
    // first byte of code in the current statement, and its line
    let mut start: Option<(usize, u32)> = None;
    let mut has_code = false;
    // nesting of ( ), since CREATE RULE bodies can hold several statements
    let mut depth = 0usize;

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
        if c == b';' && depth == 0 {
            push(sql, &mut out, start.take(), i, has_code);
            has_code = false;
            i += 1;
            continue;
        }
        start.get_or_insert((i, line));
        let end = match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                let end = sql[i..].find('\n').map_or(b.len(), |p| i + p);
                i = end;
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // block comments nest
                let mut depth = 1;
                let mut j = i + 2;
                while j < b.len() && depth > 0 {
                    if b[j..].starts_with(b"/*") {
                        depth += 1;
                        j += 2;
                    } else if b[j..].starts_with(b"*/") {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                line += sql[i..j].matches('\n').count() as u32;
                i = j;
                continue;
            }
            b'\'' => {
                // E'...' strings take backslash escapes
                let escapes =
                    i > 0 && matches!(b[i - 1], b'E' | b'e') && (i < 2 || !word_byte(b, i - 2));
                quoted_end(b, i, b'\'', escapes)
            }
            b'"' => quoted_end(b, i, b'"', false),
            b'$' => match dollar_tag(sql, i) {
                Some(tag) => sql[i + tag.len()..]
                    .find(tag)
                    .map_or(b.len(), |p| i + tag.len() + p + tag.len()),
                None => i + 1,
            },
            b'(' => {
                depth += 1;
                i + 1
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i + 1
            }
            _ => i + 1,
        };
        has_code = true;
        line += sql[i..end].matches('\n').count() as u32;
        i = end;
    }
    push(sql, &mut out, start, b.len(), has_code);
    out
}

fn push<'a>(
    sql: &'a str,
    out: &mut Vec<Statement<'a>>,
    from: Option<(usize, u32)>,
    to: usize,
    has_code: bool,
) {
    if let (Some((start, line)), true) = (from, has_code) {
        let text = sql[start..to].trim_end();
        if !text.is_empty() {
            out.push(Statement { text, start, line });
        }
    }
}

fn word_byte(b: &[u8], i: usize) -> bool {
    b.get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c >= 0x80)
}

/// Just past the closing quote; a doubled quote is an escaped one.
fn quoted_end(b: &[u8], open: usize, quote: u8, escapes: bool) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if escapes && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// `$tag$` or `$$` opening a dollar-quoted string at `i`. A `$` straight
/// after a name or number is part of it (`a$b`, `$1`), not a quote.
fn dollar_tag(sql: &str, i: usize) -> Option<&str> {
    let b = sql.as_bytes();
    if i > 0 && word_byte(b, i - 1) {
        return None;
    }
    let rest = &b[i + 1..];
    let len = rest
        .iter()
        .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c >= 0x80))?;
    if rest[len] != b'$' || rest.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    Some(&sql[i..i + len + 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        split(sql).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn splits_on_semicolons() {
        assert_eq!(texts("SELECT 1; SELECT 2;"), ["SELECT 1", "SELECT 2"]);
        assert_eq!(texts("SELECT 1"), ["SELECT 1"]);
        assert_eq!(texts(" ; ;\n"), Vec::<&str>::new());
        assert_eq!(texts("-- only a comment\n"), Vec::<&str>::new());
    }

    #[test]
    fn skips_strings_names_and_comments() {
        assert_eq!(
            texts("SELECT 'a;b', \"c;d\", E'x\\';y' /* ; /* ; */ ; */ -- ;\n; SELECT 2"),
            [
                "SELECT 'a;b', \"c;d\", E'x\\';y' /* ; /* ; */ ; */ -- ;",
                "SELECT 2"
            ]
        );
        assert_eq!(texts("SELECT 'it''s; ok'; SELECT 2").len(), 2);
        // a backslash ends nothing in a standard string
        assert_eq!(texts("SELECT 'C:\\'; SELECT 2").len(), 2);
    }

    #[test]
    fn keeps_dollar_quoted_bodies_whole() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $body$\nBEGIN\n  RETURN 1;\nEND\n$body$ LANGUAGE plpgsql;\nDO $$ BEGIN RAISE NOTICE 'x;'; END $$;\nSELECT $1, a$b FROM t";
        let s = split(sql);
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s[0].text.ends_with("LANGUAGE plpgsql"));
        assert_eq!(s[1].text, "DO $$ BEGIN RAISE NOTICE 'x;'; END $$");
        assert_eq!(s.iter().map(|s| s.line).collect::<Vec<_>>(), [1, 6, 7]);
        for st in &s {
            assert_eq!(&sql[st.start..st.start + st.text.len()], st.text);
        }
    }

    #[test]
    fn records_lines_past_comments() {
        let s = split("/* a\nb */\nSELECT 1;\n\n-- x\nSELECT\n2");
        assert_eq!(s.iter().map(|s| s.line).collect::<Vec<_>>(), [1, 5]);
        assert_eq!(s[1].text, "-- x\nSELECT\n2");
    }

    #[test]
    fn handles_unterminated_input() {
        assert_eq!(texts("SELECT 'abc; x"), ["SELECT 'abc; x"]);
        assert_eq!(texts("DO $$ BEGIN; x"), ["DO $$ BEGIN; x"]);
        assert_eq!(texts("SELECT 'ข้อความ'; SELECT 2").len(), 2);
    }
}

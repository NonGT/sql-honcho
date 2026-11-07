//! Splits a script into batches on `GO` lines, the way sqlcmd and SSMS do.
//! The server never sees `GO`; everything between two of them is one batch.

use crate::script::Statement;

pub fn split(sql: &str) -> Vec<Statement<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut i = 0;
    // first non-blank byte of the current batch, and its line
    let mut start: Option<(usize, u32)> = None;
    // comments alone don't make a batch worth sending
    let mut has_code = false;
    let mut at_line_start = true;

    while i < b.len() {
        if at_line_start {
            at_line_start = false;
            if let Some((count, eol)) = go_line(sql, i) {
                if let Some((s, l)) = start.take() {
                    if has_code {
                        let text = sql[s..i].trim_end();
                        for _ in 0..count {
                            out.push(Statement {
                                text,
                                start: s,
                                line: l,
                            });
                        }
                    }
                }
                has_code = false;
                i = eol;
                continue;
            }
        }
        let c = b[i];
        if c == b'\n' {
            line += 1;
            at_line_start = true;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        start.get_or_insert((i, line));
        match c {
            b'\'' | b'"' | b'[' => {
                has_code = true;
                let close = if c == b'[' { b']' } else { c };
                i += 1;
                while i < b.len() {
                    if b[i] == close {
                        // doubled closer is an escaped one
                        if b.get(i + 1) == Some(&close) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => i = line_end(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // T-SQL block comments nest
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        if b[i] == b'\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
            }
            _ => {
                has_code = true;
                i += 1;
            }
        }
    }
    if let Some((s, l)) = start {
        if has_code {
            out.push(Statement {
                text: sql[s..].trim_end(),
                start: s,
                line: l,
            });
        }
    }
    out
}

/// If the line starting at `i` is a `GO` separator, how many times to run
/// the batch before it (`GO 3`), and where the line ends (before its `\n`).
fn go_line(sql: &str, i: usize) -> Option<(u32, usize)> {
    let eol = sql[i..].find('\n').map_or(sql.len(), |p| i + p);
    let line = sql[i..eol].trim();
    let rest = line.get(..2).filter(|w| w.eq_ignore_ascii_case("go"))?;
    let rest = line[rest.len()..].split("--").next().unwrap_or("").trim();
    let count = match rest {
        "" => 1,
        n => n.parse().ok().filter(|&n| n > 0)?,
    };
    Some((count, eol))
}

// Words that start a T-SQL statement. FETCH and OPEN are left out because
// they also appear inside SELECT (OFFSET ... FETCH NEXT).
const STATEMENT_STARTS: &[&str] = &[
    "ALTER",
    "BACKUP",
    "BEGIN",
    "BULK",
    "CHECKPOINT",
    "COMMIT",
    "CREATE",
    "DBCC",
    "DECLARE",
    "DELETE",
    "DENY",
    "DROP",
    "EXEC",
    "EXECUTE",
    "GRANT",
    "IF",
    "INSERT",
    "KILL",
    "MERGE",
    "PRINT",
    "RAISERROR",
    "RECONFIGURE",
    "RESTORE",
    "REVOKE",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SHUTDOWN",
    "THROW",
    "TRUNCATE",
    "UPDATE",
    "USE",
    "WAITFOR",
    "WHILE",
    "WITH",
];

/// The batch cut before every word that can start a statement, for the
/// read-only check: T-SQL needs no `;` between statements, so
/// `SELECT 1 DROP TABLE t` is two of them. Subqueries get cut too, which
/// only makes the check stricter.
pub fn statements<'a>(batch: &Statement<'a>) -> Vec<Statement<'a>> {
    let sql = batch.text;
    let b = sql.as_bytes();
    let mut cuts = vec![0];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'[' => {
                let close = if c == b'[' { b']' } else { c };
                i += 1;
                while i < b.len() {
                    if b[i] == close {
                        if b.get(i + 1) == Some(&close) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => i = line_end(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = sql[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c == b'#' => {
                let start = i;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'@' | b'#' | b'$'))
                {
                    i += 1;
                }
                let word = &sql[start..i];
                let qualified = start > 0 && b[start - 1] == b'.';
                if start > 0
                    && !qualified
                    && STATEMENT_STARTS
                        .iter()
                        .any(|k| k.eq_ignore_ascii_case(word))
                {
                    cuts.push(start);
                }
            }
            _ => i += 1,
        }
    }
    cuts.push(b.len());
    cuts.windows(2)
        .filter_map(|w| {
            let text = sql[w[0]..w[1]].trim_end();
            (!text.is_empty()).then(|| Statement {
                text,
                start: batch.start + w[0],
                line: batch.line + sql[..w[0]].matches('\n').count() as u32,
            })
        })
        .collect()
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        split(sql).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn splits_on_go_lines() {
        assert_eq!(
            texts("SELECT 1; SELECT 2\nGO\nSELECT 3\n  go  \n"),
            ["SELECT 1; SELECT 2", "SELECT 3"]
        );
        assert_eq!(texts("SELECT 1"), ["SELECT 1"]);
        assert_eq!(texts("GO\n\nGO\n"), Vec::<&str>::new());
        assert_eq!(
            texts("SELECT 1\r\nGO\r\nSELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn go_only_as_a_whole_line() {
        assert_eq!(texts("SELECT 1 GO SELECT 2"), ["SELECT 1 GO SELECT 2"]);
        assert_eq!(texts("SELECT 1\nGOTO x"), ["SELECT 1\nGOTO x"]);
        assert_eq!(
            texts("SELECT 1\nGO -- next\nSELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn repeats_with_a_count() {
        assert_eq!(
            texts("INSERT t DEFAULT VALUES\nGO 3"),
            ["INSERT t DEFAULT VALUES"; 3]
        );
        // not a count, so not a separator
        assert_eq!(texts("SELECT 1\nGO x"), ["SELECT 1\nGO x"]);
    }

    #[test]
    fn ignores_go_in_strings_and_comments() {
        let sql = "SELECT 'a\nGO\nb'\nGO\nSELECT [x\nGO\n]]y]\n/* /* \nGO\n */ \nGO\n */\nSELECT 2";
        assert_eq!(
            texts(sql),
            [
                "SELECT 'a\nGO\nb'",
                "SELECT [x\nGO\n]]y]\n/* /* \nGO\n */ \nGO\n */\nSELECT 2"
            ]
        );
        assert_eq!(
            texts("SELECT 'it''s'\nGO\nSELECT 2"),
            ["SELECT 'it''s'", "SELECT 2"]
        );
    }

    #[test]
    fn drops_comment_only_batches() {
        assert_eq!(texts("SELECT 1\nGO\n-- done\n"), ["SELECT 1"]);
        assert_eq!(texts("/* a */\nGO\n-- b"), Vec::<&str>::new());
    }

    #[test]
    fn records_start_lines() {
        let sql = "SELECT 1\nGO\n\n  SELECT\n'a\nb'\nGO\n-- c\nSELECT 3";
        let s = split(sql);
        let lines: Vec<u32> = s.iter().map(|s| s.line).collect();
        assert_eq!(lines, [1, 4, 8]);
        for st in &s {
            assert_eq!(&sql[st.start..st.start + st.text.len()], st.text);
        }
    }

    fn checked(sql: &str) -> Vec<&str> {
        split(sql)
            .iter()
            .flat_map(statements)
            .map(|s| s.text)
            .collect()
    }

    #[test]
    fn cuts_batches_at_statement_words_for_the_read_only_check() {
        assert_eq!(
            checked("SELECT 1 DROP TABLE t"),
            ["SELECT 1", "DROP TABLE t"]
        );
        assert_eq!(
            checked("SELECT a FROM t WHERE b IN (SELECT c FROM u)"),
            ["SELECT a FROM t WHERE b IN (", "SELECT c FROM u)"]
        );
        // not in strings, names or comments, nor as part of a longer name
        assert_eq!(
            checked("SELECT 'drop', [delete], set_id, @update /* exec */ FROM t"),
            ["SELECT 'drop', [delete], set_id, @update /* exec */ FROM t"]
        );
        assert_eq!(
            checked("SELECT 1\nGO\nUPDATE t SET a = 1"),
            ["SELECT 1", "UPDATE t", "SET a = 1"]
        );
        let s: Vec<_> = split("SELECT 1\nGO\nSELECT 2\nDELETE t")
            .iter()
            .flat_map(statements)
            .collect();
        assert_eq!(s.iter().map(|s| s.line).collect::<Vec<_>>(), [1, 3, 4]);
        for st in &s {
            assert_eq!(
                &"SELECT 1\nGO\nSELECT 2\nDELETE t"[st.start..st.start + st.text.len()],
                st.text
            );
        }
    }

    #[test]
    fn handles_unterminated_input() {
        assert_eq!(texts("SELECT 'abc\nGO"), ["SELECT 'abc\nGO"]);
        assert_eq!(texts("SELECT 1 /* open\nGO"), ["SELECT 1 /* open\nGO"]);
        assert_eq!(
            texts("SELECT N'ข้อความ'\nGO\nSELECT 2"),
            ["SELECT N'ข้อความ'", "SELECT 2"]
        );
    }
}

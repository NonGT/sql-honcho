//! Statements of a script, and telling reads from writes for read-only
//! connections.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement<'a> {
    pub text: &'a str,
    /// Byte offset of `text` in the script.
    pub start: usize,
    /// 1-based line the statement starts on.
    pub line: u32,
}

impl<'a> Statement<'a> {
    pub fn whole(sql: &'a str) -> Self {
        Self {
            text: sql,
            start: 0,
            line: 1,
        }
    }
}

/// Splits a script the way the driver for `kind` will when it runs it.
pub fn split(kind: crate::DriverKind, sql: &str) -> Vec<Statement<'_>> {
    use crate::DriverKind::*;
    match kind {
        MySql | Fake => crate::mysql::split(sql),
        SqlServer => crate::sqlserver::split(sql),
        // each driver brings its own splitter when it lands
        Postgres | Sqlite => vec![Statement::whole(sql)],
    }
}

/// Names the statement kind when it changes every row of a table: UPDATE
/// or DELETE without a WHERE.
pub fn unfiltered_write(statement: &str) -> Option<&'static str> {
    let words = words(statement);
    let kind = match words.first()?.0.to_ascii_uppercase().as_str() {
        "UPDATE" => "UPDATE",
        "DELETE" => "DELETE",
        _ => return None,
    };
    (!words.iter().any(|(w, _)| w.eq_ignore_ascii_case("WHERE"))).then_some(kind)
}

const READS: &[&str] = &[
    "DESC", "DESCRIBE", "EXPLAIN", "HELP", "SELECT", "SHOW", "TABLE", "USE", "VALUES", "WITH",
];

// Words that make a read statement write or lock something: WITH ...
// DELETE, SELECT ... INTO, SELECT ... FOR UPDATE, EXPLAIN ANALYZE (which
// runs the statement). Followed by `(` they are functions and fine.
const WRITES: &[&str] = &[
    "ANALYZE", "DELETE", "INSERT", "INTO", "LOCK", "MERGE", "REPLACE", "UPDATE",
];

/// Why a statement isn't a plain read, or `None` if it is one.
///
/// Errs on the side of refusing: it only looks at keywords, so anything it
/// isn't sure about counts as a write. Functions with side effects get
/// through, which is why drivers also make the session read-only on the
/// server where they can.
pub fn write_reason(statement: &str) -> Option<String> {
    let words = words(statement);
    let (first, _) = words.first()?;
    let first = first.to_ascii_uppercase();
    if !READS.contains(&first.as_str()) {
        return Some(format!("{first} isn't a read"));
    }
    words
        .iter()
        .find(|(w, call)| !call && WRITES.iter().any(|k| w.eq_ignore_ascii_case(k)))
        .map(|(w, _)| format!("{first} with {} can write", w.to_ascii_uppercase()))
}

/// Bare words outside strings, quoted names and comments, each with whether
/// a `(` follows it.
fn words(s: &str) -> Vec<(&str, bool)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' | b'[' => {
                let close = if c == b'[' { b']' } else { c };
                i += 1;
                while i < b.len() && b[i] != close {
                    if b[i] == b'\\' && c == b'\'' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = s[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80)
                {
                    i += 1;
                }
                let word = &s[start..i];
                let call = s[i..].trim_start().starts_with('(');
                out.push((word, call));
            }
            _ if c.is_ascii_whitespace() => i += 1,
            _ => {
                i += 1;
                // skip the rest of a multi-byte char or a number
                while i < b.len() && (b[i].is_ascii_digit() || (0x80..0xc0).contains(&b[i])) {
                    i += 1;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(s: &str) -> bool {
        write_reason(s).is_none()
    }

    #[test]
    fn reads() {
        assert!(read("SELECT * FROM users WHERE name = 'DELETE'"));
        assert!(read(
            "select replace(name, 'a', 'b'), insert ('x', 1, 1, 'y') from t"
        ));
        assert!(read("  -- DELETE everything\n  SELECT 1 /* UPDATE */"));
        assert!(read("SHOW TABLES"));
        assert!(read("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(read("EXPLAIN SELECT * FROM `update`"));
        assert!(read("SELECT [into] FROM [delete]"));
        assert!(read("SELECT 'it\\'s DELETE' AS x"));
        assert!(read("SELECT \"ชื่อ\", ข้อมูล FROM t"));
        assert!(read(""));
    }

    #[test]
    fn unfiltered_writes() {
        assert_eq!(unfiltered_write("DELETE FROM users"), Some("DELETE"));
        assert_eq!(unfiltered_write("update t set a = 'where'"), Some("UPDATE"));
        assert_eq!(
            unfiltered_write("UPDATE t SET a = 1 -- WHERE id = 1"),
            Some("UPDATE")
        );
        assert_eq!(unfiltered_write("DELETE FROM t WHERE id = 1"), None);
        assert_eq!(unfiltered_write("update t set a = 1\nwhere id = 2"), None);
        assert_eq!(unfiltered_write("SELECT * FROM t"), None);
        assert_eq!(unfiltered_write(""), None);
    }

    #[test]
    fn writes() {
        for s in [
            "DELETE FROM users",
            "update users set a = 1",
            "INSERT INTO t VALUES (1)",
            "CREATE TABLE t (x INT)",
            "DROP TABLE t",
            "CALL refresh_stats()",
            "SET GLOBAL read_only = 0",
            "WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone",
            "SELECT * INTO backup FROM users",
            "SELECT * FROM users FOR UPDATE",
            "EXPLAIN ANALYZE DELETE FROM t",
            "(SELECT 1) UNION (SELECT 2) INTO OUTFILE '/tmp/x'",
            "SELECT 1; DELETE FROM t",
        ] {
            assert!(!read(s), "{s} passed as a read");
        }
        assert_eq!(
            write_reason("delete from t").as_deref(),
            Some("DELETE isn't a read")
        );
        assert_eq!(
            write_reason("SELECT * FROM t FOR UPDATE").as_deref(),
            Some("SELECT with UPDATE can write")
        );
    }
}

//! Splits a script into statements the way the `mysql` client does,
//! including `DELIMITER` lines, which the server doesn't understand.

use crate::script::Statement;

pub fn split(sql: &str) -> Vec<Statement<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut delimiter = ";";
    let mut line = 1u32;
    let mut i = 0;
    // first non-blank byte of the current statement, and its line
    let mut start: Option<(usize, u32)> = None;
    // comments alone don't make a statement worth sending
    let mut has_code = false;

    let mut push = |start: &mut Option<(usize, u32)>, end: usize, has_code: &mut bool| {
        if let Some((s, l)) = start.take() {
            if *has_code {
                out.push(Statement {
                    text: sql[s..end].trim_end(),
                    line: l,
                });
            }
        }
        *has_code = false;
    };

    while i < b.len() {
        if !has_code && is_delimiter_command(&b[i..]) {
            let eol = sql[i..].find('\n').map_or(b.len(), |p| i + p);
            if let Some(d) = sql[i + 9..eol].split_whitespace().next() {
                delimiter = d;
            }
            start = None;
            i = eol;
            continue;
        }
        if b[i..].starts_with(delimiter.as_bytes()) {
            push(&mut start, i, &mut has_code);
            i += delimiter.len();
            continue;
        }
        let c = b[i];
        if c == b'\n' {
            line += 1;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        start.get_or_insert((i, line));
        match c {
            b'\'' | b'"' | b'`' => {
                has_code = true;
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' && c != b'`' {
                        i += 1;
                    }
                    if b.get(i) == Some(&b'\n') {
                        line += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'#' => i = line_end(b, i),
            b'-' if b.get(i + 1) == Some(&b'-')
                && b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace()) =>
            {
                i = line_end(b, i)
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // /*! ... */ is code that MySQL runs
                if b.get(i + 2) == Some(&b'!') {
                    has_code = true;
                }
                i += 2;
                while i < b.len() && !b[i..].starts_with(b"*/") {
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            _ => {
                has_code = true;
                i += 1;
            }
        }
    }
    push(&mut start, b.len(), &mut has_code);
    out
}

fn is_delimiter_command(b: &[u8]) -> bool {
    b.len() > 9 && b[..9].eq_ignore_ascii_case(b"delimiter") && b[9].is_ascii_whitespace()
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
    fn splits_on_semicolons() {
        assert_eq!(texts("SELECT 1; SELECT 2;"), ["SELECT 1", "SELECT 2"]);
        assert_eq!(texts("SELECT 1"), ["SELECT 1"]);
        assert_eq!(texts(" ;; \n "), Vec::<&str>::new());
    }

    #[test]
    fn ignores_semicolons_in_strings_and_comments() {
        let sql = "SELECT 'a;b', \"c;d\", `e;f`; -- x;y\nSELECT 'it\\'s;' # z;\n/* ; */ SELECT 3";
        assert_eq!(
            texts(sql),
            [
                "SELECT 'a;b', \"c;d\", `e;f`",
                "-- x;y\nSELECT 'it\\'s;' # z;\n/* ; */ SELECT 3"
            ]
        );
        assert_eq!(
            texts("SELECT 'a'';'; SELECT 2"),
            ["SELECT 'a'';'", "SELECT 2"]
        );
        // `--` needs a space after it to be a comment
        assert_eq!(texts("SELECT 1--1; SELECT 2"), ["SELECT 1--1", "SELECT 2"]);
    }

    #[test]
    fn drops_comment_only_statements() {
        assert_eq!(texts("SELECT 1; -- done\n"), ["SELECT 1"]);
        assert_eq!(texts("/* a */; # b"), Vec::<&str>::new());
        assert_eq!(
            texts("/*!40101 SET NAMES utf8 */;"),
            ["/*!40101 SET NAMES utf8 */"]
        );
    }

    #[test]
    fn follows_delimiter_lines() {
        let sql = "DELIMITER //\n\
                   CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END //\n\
                   delimiter ;\n\
                   CALL p();";
        assert_eq!(
            texts(sql),
            [
                "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END",
                "CALL p()"
            ]
        );
        assert_eq!(
            texts("DELIMITER $$\nSELECT 1$$SELECT 2$$"),
            ["SELECT 1", "SELECT 2"]
        );
        // only a command at the start of a statement
        assert_eq!(
            texts("SELECT delimiter FROM t"),
            ["SELECT delimiter FROM t"]
        );
    }

    #[test]
    fn records_start_lines() {
        let s = split("SELECT 1;\n\n  SELECT\n'a\nb';\n-- c\nSELECT 3");
        let lines: Vec<u32> = s.iter().map(|s| s.line).collect();
        assert_eq!(lines, [1, 3, 6]);
    }

    #[test]
    fn handles_unterminated_input() {
        assert_eq!(texts("SELECT 'abc"), ["SELECT 'abc"]);
        assert_eq!(texts("SELECT 1 /* open"), ["SELECT 1 /* open"]);
        assert_eq!(
            texts("SELECT 'ข้อความ;'; SELECT 2"),
            ["SELECT 'ข้อความ;'", "SELECT 2"]
        );
    }
}

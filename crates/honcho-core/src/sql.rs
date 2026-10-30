use std::ops::Range;

use ropey::Rope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Type,
    Function,
    Identifier,
    QuotedIdent,
    String,
    Number,
    Comment,
    Operator,
    Punctuation,
    Variable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Byte range within the line.
    pub range: Range<usize>,
    pub kind: TokenKind,
}

/// Lexer state carried from one line to the next.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum LineState {
    #[default]
    Normal,
    BlockComment,
    Str(char),
    /// Postgres `$tag$ ... $tag$` body, holding the full delimiter.
    Dollar(String),
}

const KEYWORDS: &[&str] = &[
    "ADD", "AFTER", "AGGREGATE", "ALL", "ALTER", "ANALYZE", "AND", "ANY", "AS", "ASC",
    "AUTO_INCREMENT", "BEFORE", "BEGIN", "BETWEEN", "BREAK", "BY", "CALL", "CASCADE", "CASE",
    "CAST", "CATCH", "CHECK", "CLOSE", "CLUSTERED", "COLLATE", "COLUMN", "COMMENT", "COMMIT",
    "CONCURRENTLY", "CONFLICT", "CONSTRAINT", "CONTINUE", "CREATE", "CROSS", "CTE", "CURSOR",
    "DATABASE", "DEALLOCATE", "DECLARE", "DEFAULT", "DEFERRABLE", "DELETE", "DELIMITER", "DESC",
    "DESCRIBE", "DETERMINISTIC", "DISTINCT", "DO", "DROP", "EACH", "ELSE", "ELSEIF", "ELSIF", "END",
    "ENGINE", "ESCAPE", "EXCEPT", "EXCEPTION", "EXEC", "EXECUTE", "EXISTS", "EXPLAIN", "EXTENSION",
    "FALSE", "FETCH", "FILTER", "FIRST", "FOLLOWING", "FOR", "FOREIGN", "FROM", "FULL", "FUNCTION",
    "GO", "GOTO", "GRANT", "GROUP", "HAVING", "IDENTITY", "IF", "ILIKE", "IMMUTABLE", "IN", "INDEX",
    "INNER", "INOUT", "INSERT", "INSTEAD", "INTERSECT", "INTO", "IS", "ISNULL", "JOIN", "KEY",
    "LANGUAGE", "LAST", "LATERAL", "LEAVE", "LEFT", "LIKE", "LIMIT", "LOOP", "MATERIALIZED",
    "MERGE", "MODIFY", "NATURAL", "NEXT", "NO", "NOCOUNT", "NONCLUSTERED", "NOT", "NOTNULL",
    "NOWAIT", "NULL", "NULLS", "OF", "OFF", "OFFSET", "ON", "ONLY", "OPEN", "OR", "ORDER", "OUT",
    "OUTER", "OUTPUT", "OVER", "OWNER", "PARTITION", "PERFORM", "PRECEDING", "PRIMARY", "PRINT",
    "PROC", "PROCEDURE", "RAISE", "RAISERROR", "RANGE", "READS", "RECURSIVE", "REFERENCES",
    "REFRESH", "RENAME", "REPEAT", "REPLACE", "RESTRICT", "RETURN", "RETURNING", "RETURNS",
    "REVOKE", "RIGHT", "ROLE", "ROLLBACK", "ROW", "ROWS", "SAVEPOINT", "SCHEMA", "SECURITY",
    "SELECT", "SEQUENCE", "SET", "SETOF", "SHOW", "SIGNAL", "SOME", "STABLE", "STRICT", "TABLE",
    "TEMP", "TEMPORARY", "THEN", "THROW", "TO", "TOP", "TRAN", "TRANSACTION", "TRIGGER", "TRUE",
    "TRUNCATE", "TRY", "UNBOUNDED", "UNION", "UNIQUE", "UNKNOWN", "UNTIL", "UPDATE", "USE", "USING",
    "VACUUM", "VALUES", "VIEW", "VOLATILE", "WAITFOR", "WHEN", "WHERE", "WHILE", "WINDOW", "WITH",
    "WITHIN", "WITHOUT",
];

const TYPES: &[&str] = &[
    "BIGINT", "BIGSERIAL", "BINARY", "BIT", "BLOB", "BOOL", "BOOLEAN", "BYTEA", "CHAR", "CHARACTER",
    "CIDR", "DATE", "DATETIME", "DATETIME2", "DATETIMEOFFSET", "DEC", "DECIMAL", "DOUBLE", "ENUM",
    "FLOAT", "IMAGE", "INET", "INT", "INT2", "INT4", "INT8", "INTEGER", "INTERVAL", "JSON", "JSONB",
    "LONGBLOB", "LONGTEXT", "MEDIUMINT", "MEDIUMTEXT", "MONEY", "NCHAR", "NTEXT", "NUMERIC", "NVARCHAR",
    "PRECISION", "REAL", "RECORD", "REGCLASS", "SERIAL", "SMALLDATETIME", "SMALLINT", "SMALLMONEY",
    "SMALLSERIAL", "SQL_VARIANT", "TEXT", "TIME", "TIMESTAMP", "TIMESTAMPTZ", "TIMETZ", "TINYINT",
    "TINYTEXT", "UNIQUEIDENTIFIER", "UNSIGNED", "UUID", "VARBINARY", "VARCHAR", "VARYING", "VOID",
    "XML", "YEAR", "ZONE",
];

/// Keywords that are really functions when called with parentheses.
const CALLABLE_KEYWORDS: &[&str] = &["LEFT", "RIGHT", "REPLACE", "IF", "INSERT"];

fn upper_in(word: &str, list: &[&str]) -> bool {
    if word.len() > 20 || !word.is_ascii() {
        return false;
    }
    let up = word.to_ascii_uppercase();
    list.binary_search(&up.as_str()).is_ok()
}

pub fn is_keyword(word: &str) -> bool {
    upper_in(word, KEYWORDS)
}

pub fn is_type(word: &str) -> bool {
    upper_in(word, TYPES)
}

fn ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '#'
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '#'
}

fn quote_kind(q: char) -> TokenKind {
    if q == '\'' {
        TokenKind::String
    } else {
        TokenKind::QuotedIdent
    }
}

/// Scan from just after an opening quote. Returns the end (past the closing
/// quote, or the line end) and whether the quote was closed.
fn scan_quoted(bytes: &[u8], mut i: usize, q: u8, backslash: bool) -> (usize, bool) {
    while i < bytes.len() {
        let b = bytes[i];
        if backslash && b == b'\\' {
            i += 2;
            continue;
        }
        if b == q {
            if bytes.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return (i + 1, true);
        }
        i += 1;
    }
    (bytes.len(), false)
}

/// Tokenize one line (without its newline), starting from `state`.
/// Tokens are passed to `emit`; the state at the end of the line is returned.
pub fn lex_line(line: &str, mut state: LineState, mut emit: impl FnMut(Token)) -> LineState {
    let bytes = line.as_bytes();
    let len = line.len();
    let mut i = 0;
    let mut push = |start: usize, end: usize, kind: TokenKind| {
        if end > start {
            emit(Token { range: start..end, kind });
        }
    };

    while i < len {
        match &state {
            LineState::BlockComment => {
                match line[i..].find("*/") {
                    Some(p) => {
                        push(i, i + p + 2, TokenKind::Comment);
                        i += p + 2;
                        state = LineState::Normal;
                    }
                    None => {
                        push(i, len, TokenKind::Comment);
                        i = len;
                    }
                }
                continue;
            }
            LineState::Str(q) => {
                let q = *q;
                let (end, closed) = scan_quoted(bytes, i, q as u8, false);
                push(i, end, quote_kind(q));
                i = end;
                if closed {
                    state = LineState::Normal;
                }
                continue;
            }
            LineState::Dollar(tag) => {
                match line[i..].find(tag.as_str()) {
                    Some(p) => {
                        let end = i + p + tag.len();
                        push(i, end, TokenKind::String);
                        i = end;
                        state = LineState::Normal;
                    }
                    None => {
                        push(i, len, TokenKind::String);
                        i = len;
                    }
                }
                continue;
            }
            LineState::Normal => {}
        }

        let rest = &line[i..];
        let c = rest.chars().next().unwrap();
        let next = rest[c.len_utf8()..].chars().next();

        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if rest.starts_with("--") {
            push(i, len, TokenKind::Comment);
            break;
        }
        if rest.starts_with("/*") {
            match line[i + 2..].find("*/") {
                Some(p) => {
                    push(i, i + 2 + p + 2, TokenKind::Comment);
                    i += 2 + p + 2;
                }
                None => {
                    push(i, len, TokenKind::Comment);
                    state = LineState::BlockComment;
                    break;
                }
            }
            continue;
        }
        let prefixed = matches!(c, 'N' | 'n' | 'E' | 'e') && next == Some('\'');
        if c == '\'' || c == '"' || c == '`' || prefixed {
            let q = if prefixed { '\'' } else { c };
            let skip = if prefixed { 2 } else { 1 };
            let backslash = prefixed && matches!(c, 'E' | 'e');
            let (end, closed) = scan_quoted(bytes, i + skip, q as u8, backslash);
            push(i, end, quote_kind(q));
            i = end;
            if !closed {
                state = LineState::Str(q);
            }
            continue;
        }
        if c == '$' {
            // $1 positional parameter
            if next.is_some_and(|n| n.is_ascii_digit()) {
                let start = i;
                i += 1;
                while i < len && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                push(start, i, TokenKind::Variable);
                continue;
            }
            // $tag$ dollar quote
            let tag_len = rest[1..]
                .char_indices()
                .find(|(_, ch)| !(ch.is_alphanumeric() || *ch == '_'))
                .map(|(p, ch)| if ch == '$' { Some(p) } else { None })
                .unwrap_or(None);
            if let Some(tl) = tag_len {
                let tag = rest[..tl + 2].to_string();
                let start = i;
                i += tag.len();
                match line[i..].find(tag.as_str()) {
                    Some(p) => {
                        i += p + tag.len();
                        push(start, i, TokenKind::String);
                    }
                    None => {
                        push(start, len, TokenKind::String);
                        state = LineState::Dollar(tag);
                        break;
                    }
                }
                continue;
            }
        }
        if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) {
            let start = i;
            if rest.starts_with("0x") || rest.starts_with("0X") {
                i += 2;
                while i < len && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if i < len && (bytes[i] == b'e' || bytes[i] == b'E') {
                    let mut j = i + 1;
                    if j < len && (bytes[j] == b'+' || bytes[j] == b'-') {
                        j += 1;
                    }
                    if j < len && bytes[j].is_ascii_digit() {
                        i = j;
                        while i < len && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
            }
            push(start, i, TokenKind::Number);
            continue;
        }
        if c == '@' || (c == ':' && next.is_some_and(ident_start)) {
            let start = i;
            i += 1;
            if c == '@' && next == Some('@') {
                i += 1;
            }
            while let Some(ch) = line[i..].chars().next().filter(|ch| ident_char(*ch)) {
                i += ch.len_utf8();
            }
            push(start, i, TokenKind::Variable);
            continue;
        }
        if c == '[' {
            if let Some(p) = rest.find(']') {
                let inner = &rest[1..p];
                if !inner.is_empty() && inner.chars().all(|ch| ident_char(ch) || ch == ' ' || ch == '-' || ch == '.') {
                    push(i, i + p + 1, TokenKind::QuotedIdent);
                    i += p + 1;
                    continue;
                }
            }
        }
        if ident_start(c) {
            let start = i;
            while let Some(ch) = line[i..].chars().next().filter(|ch| ident_char(*ch)) {
                i += ch.len_utf8();
            }
            let word = &line[start..i];
            let called = line[i..].trim_start().starts_with('(');
            let after_dot = line[..start].ends_with('.');
            let kind = if after_dot {
                if called { TokenKind::Function } else { TokenKind::Identifier }
            } else if is_type(word) {
                TokenKind::Type
            } else if is_keyword(word) {
                if called && upper_in(word, CALLABLE_KEYWORDS) {
                    TokenKind::Function
                } else {
                    TokenKind::Keyword
                }
            } else if called {
                TokenKind::Function
            } else {
                TokenKind::Identifier
            };
            push(start, i, kind);
            continue;
        }
        let start = i;
        i += c.len_utf8();
        let kind = if "+-*/%=<>!|&^~:".contains(c) {
            while i < len && "=<>|&:".contains(bytes[i] as char) {
                i += 1;
            }
            TokenKind::Operator
        } else {
            TokenKind::Punctuation
        };
        push(start, i, kind);
    }
    state
}

/// Caches the lexer state at the start of each line so only edited and
/// visible lines get re-lexed.
#[derive(Default)]
pub struct Highlighter {
    states: Vec<LineState>,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            states: vec![LineState::Normal],
        }
    }

    /// Lines from `line` onward may have changed.
    pub fn invalidate_from(&mut self, line: usize) {
        self.states.truncate(line + 1);
        if self.states.is_empty() {
            self.states.push(LineState::Normal);
        }
    }

    pub fn line_tokens(&mut self, rope: &Rope, line: usize) -> Vec<Token> {
        let line = line.min(rope.len_lines().saturating_sub(1));
        while self.states.len() <= line {
            let i = self.states.len() - 1;
            let text = line_string(rope, i);
            let end = lex_line(&text, self.states[i].clone(), |_| {});
            self.states.push(end);
        }
        let mut tokens = Vec::new();
        lex_line(&line_string(rope, line), self.states[line].clone(), |t| tokens.push(t));
        tokens
    }
}

pub fn line_string(rope: &Rope, line: usize) -> String {
    let mut s = rope.line(line).to_string();
    if s.ends_with('\n') {
        s.pop();
    }
    s
}

/// Char range of the statement around `offset`, split on `;` outside
/// strings and comments. Empty statements are skipped.
pub fn statement_at(rope: &Rope, offset: usize) -> Option<Range<usize>> {
    statements(rope)
        .into_iter()
        .find(|r| offset >= r.start && offset <= r.end)
        .or_else(|| statements(rope).into_iter().rev().find(|r| r.end <= offset))
}

/// All statements in char offsets, trimmed of surrounding whitespace.
pub fn statements(rope: &Rope) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut state = LineState::Normal;
    let mut stmt_start = 0usize;
    let mut line_char_start = 0usize;
    for li in 0..rope.len_lines() {
        let text = line_string(rope, li);
        let mut code_semis = Vec::new();
        let end_state = lex_line(&text, state.clone(), |t| {
            if t.kind == TokenKind::Punctuation && &text[t.range.clone()] == ";" {
                code_semis.push(t.range.start);
            }
        });
        for b in code_semis {
            let at = line_char_start + text[..b].chars().count();
            out.push(stmt_start..at);
            stmt_start = at + 1;
        }
        state = end_state;
        line_char_start += rope.line(li).len_chars();
    }
    out.push(stmt_start..rope.len_chars());

    out.into_iter()
        .filter_map(|r| {
            let s = rope.slice(r.clone()).to_string();
            let lead = s.chars().take_while(|c| c.is_whitespace()).count();
            let trail = s.chars().rev().take_while(|c| c.is_whitespace()).count();
            let total = s.chars().count();
            (lead < total).then(|| r.start + lead..r.end - trail)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<(String, TokenKind)> {
        let mut out = Vec::new();
        lex_line(line, LineState::Normal, |t| out.push((line[t.range].to_string(), t.kind)));
        out
    }

    #[test]
    fn lists_are_sorted() {
        let mut k = KEYWORDS.to_vec();
        k.sort();
        assert_eq!(k, KEYWORDS);
        let mut t = TYPES.to_vec();
        t.sort();
        assert_eq!(t, TYPES);
    }

    #[test]
    fn basic_select() {
        let k = kinds("SELECT id, count(*) FROM users WHERE name = 'bob' -- hi");
        assert_eq!(k[0], ("SELECT".into(), TokenKind::Keyword));
        assert_eq!(k[1], ("id".into(), TokenKind::Identifier));
        assert_eq!(k[3], ("count".into(), TokenKind::Function));
        assert!(k.contains(&("'bob'".into(), TokenKind::String)));
        assert_eq!(k.last().unwrap(), &("-- hi".into(), TokenKind::Comment));
    }

    #[test]
    fn types_and_variables() {
        let k = kinds("DECLARE @x NVARCHAR(50) = N'สวัสดี'; SELECT $1, :name, a::int");
        assert!(k.contains(&("@x".into(), TokenKind::Variable)));
        assert!(k.contains(&("NVARCHAR".into(), TokenKind::Type)));
        assert!(k.contains(&("N'สวัสดี'".into(), TokenKind::String)));
        assert!(k.contains(&("$1".into(), TokenKind::Variable)));
        assert!(k.contains(&(":name".into(), TokenKind::Variable)));
        assert!(k.contains(&("::".into(), TokenKind::Operator)));
    }

    #[test]
    fn multiline_states() {
        let s = lex_line("SELECT /* start", LineState::Normal, |_| {});
        assert_eq!(s, LineState::BlockComment);
        let s = lex_line("still */ 1", s, |_| {});
        assert_eq!(s, LineState::Normal);

        let s = lex_line("AS $$", LineState::Normal, |_| {});
        assert_eq!(s, LineState::Dollar("$$".into()));
        let s = lex_line("BEGIN RETURN 1; END; $$ LANGUAGE plpgsql;", s, |_| {});
        assert_eq!(s, LineState::Normal);

        let s = lex_line("SELECT 'multi", LineState::Normal, |_| {});
        assert_eq!(s, LineState::Str('\''));
        let s = lex_line("line' AS x", s, |_| {});
        assert_eq!(s, LineState::Normal);
    }

    #[test]
    fn escaped_quotes() {
        let k = kinds("SELECT 'it''s' AS [Full Name]");
        assert!(k.contains(&("'it''s'".into(), TokenKind::String)));
        assert!(k.contains(&("[Full Name]".into(), TokenKind::QuotedIdent)));
    }

    #[test]
    fn highlighter_tracks_block_comments() {
        let rope = Rope::from_str("/*\nSELECT\n*/\nSELECT");
        let mut h = Highlighter::new();
        assert_eq!(h.line_tokens(&rope, 1)[0].kind, TokenKind::Comment);
        assert_eq!(h.line_tokens(&rope, 3)[0].kind, TokenKind::Keyword);
    }

    #[test]
    fn statement_splitting() {
        let rope = Rope::from_str("SELECT 1;\n\nSELECT ';' ; -- x;\nUPDATE t SET a = 1");
        let st = statements(&rope);
        let texts: Vec<String> = st.iter().map(|r| rope.slice(r.clone()).to_string()).collect();
        assert_eq!(texts, vec!["SELECT 1", "SELECT ';'", "-- x;\nUPDATE t SET a = 1"]);
        let at = statement_at(&rope, rope.len_chars() - 1).unwrap();
        assert_eq!(rope.slice(at).to_string(), "-- x;\nUPDATE t SET a = 1");
    }
}

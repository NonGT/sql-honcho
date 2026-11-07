//! SQL completion: works out what can go at the cursor from the statement
//! around it and a schema, without a database or a UI.

use std::ops::Range;

use crate::sql::KEYWORDS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Column,
    Alias,
    Table,
    View,
    Database,
    Schema,
    Procedure,
    Function,
    Keyword,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaObject {
    /// On servers with schemas.
    pub schema: Option<String>,
    pub name: String,
    pub kind: Kind,
    /// Parameters and return type for routines.
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaColumn {
    pub name: String,
    pub data_type: String,
}

/// The SQL being written: how names are quoted and qualified.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dialect {
    /// `db.table`, backticks, `#` comments and backslash escapes.
    #[default]
    MySql,
    /// `db.schema.table`, [brackets], and `#` starts a temp table name.
    SqlServer,
    /// `schema.table`, "double quotes", and $$ quoted bodies. Unquoted
    /// names fold to lower case.
    Postgres,
    /// `db.table` across attached files, "double quotes" (or [brackets]
    /// or backticks), no schemas.
    Sqlite,
}

impl Dialect {
    /// The schema unqualified names resolve in, on servers with schemas.
    fn default_schema(self) -> Option<&'static str> {
        match self {
            Dialect::MySql => None,
            Dialect::SqlServer => Some("dbo"),
            Dialect::Postgres => Some("public"),
            Dialect::Sqlite => None,
        }
    }

    fn has_schemas(self) -> bool {
        self.default_schema().is_some()
    }
}

/// What the completer knows about the server. `None` means not loaded
/// yet, which the result reports so the caller can fetch it.
pub trait Schema {
    fn dialect(&self) -> Dialect {
        Dialect::MySql
    }
    fn current_database(&self) -> Option<&str>;
    fn databases(&self) -> Option<&[String]>;
    /// Every object in the database, in all its schemas.
    fn objects(&self, database: &str) -> Option<&[SchemaObject]>;
    /// A table's columns; without a schema, the one the server would pick.
    fn columns(&self, database: &str, schema: Option<&str>, table: &str)
        -> Option<&[SchemaColumn]>;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Need {
    Databases,
    Objects(String),
    /// Database, schema and table.
    Columns(String, Option<String>, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub kind: Kind,
    pub detail: Option<String>,
    /// What replaces the typed prefix: quoted when the name needs it, and
    /// keywords in the case the user is typing in.
    pub insert: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completions {
    /// Byte range in the statement that `insert` replaces.
    pub replace: Range<usize>,
    pub items: Vec<Candidate>,
    /// Schema the answer would have used but which isn't loaded.
    pub missing: Vec<Need>,
}

const MAX_ITEMS: usize = 100;

/// Common built-in functions offered where an expression can go.
const FUNCTIONS: &[&str] = &[
    "ABS",
    "AGE",
    "ARRAY_AGG",
    "AVG",
    "CAST",
    "CEIL",
    "CEILING",
    "COALESCE",
    "CONCAT",
    "CONCAT_WS",
    "CONVERT",
    "COUNT",
    "CURDATE",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DATE",
    "DATEADD",
    "DATEDIFF",
    "DATEPART",
    "DATETIME",
    "DATE_ADD",
    "DATE_FORMAT",
    "DATE_SUB",
    "DATE_TRUNC",
    "EOMONTH",
    "EXISTS",
    "EXTRACT",
    "FLOOR",
    "FORMAT",
    "GENERATE_SERIES",
    "GEN_RANDOM_UUID",
    "GETDATE",
    "GREATEST",
    "GROUP_CONCAT",
    "IF",
    "IFNULL",
    "IIF",
    "INSTR",
    "ISNULL",
    "JSONB_BUILD_OBJECT",
    "JSON_ARRAY",
    "JSON_EXTRACT",
    "JSON_OBJECT",
    "JSON_UNQUOTE",
    "JSON_VALUE",
    "JULIANDAY",
    "LEAST",
    "LEFT",
    "LEN",
    "LENGTH",
    "LOWER",
    "LPAD",
    "LTRIM",
    "MAX",
    "MIN",
    "MOD",
    "NEWID",
    "NOW",
    "NULLIF",
    "POSITION",
    "PRINTF",
    "RAND",
    "RANDOM",
    "REGEXP_REPLACE",
    "REPLACE",
    "RIGHT",
    "ROUND",
    "RPAD",
    "RTRIM",
    "SPLIT_PART",
    "STRFTIME",
    "STRING_AGG",
    "STR_TO_DATE",
    "SUBSTRING",
    "SUM",
    "SYSDATETIME",
    "TIMESTAMPDIFF",
    "TO_CHAR",
    "TO_DATE",
    "TRIM",
    "TRUNCATE",
    "TRY_CAST",
    "TYPEOF",
    "UPPER",
    "UUID",
    "YEAR",
];

/// Whether a built-in function exists in the dialect; those not listed
/// are everyone's.
fn function_in(f: &str, dialect: Dialect) -> bool {
    use Dialect::*;
    let only: &[Dialect] = match f {
        "CURDATE" | "DATE_ADD" | "DATE_FORMAT" | "DATE_SUB" | "IF" | "JSON_UNQUOTE"
        | "STR_TO_DATE" | "TIMESTAMPDIFF" | "TRUNCATE" | "UUID" => &[MySql],
        "CEILING" | "DATEADD" | "DATEPART" | "EOMONTH" | "GETDATE" | "ISNULL" | "LEN" | "NEWID"
        | "SYSDATETIME" | "TRY_CAST" => &[SqlServer],
        "AGE" | "ARRAY_AGG" | "DATE_TRUNC" | "GENERATE_SERIES" | "GEN_RANDOM_UUID"
        | "JSONB_BUILD_OBJECT" | "SPLIT_PART" | "TO_CHAR" | "TO_DATE" => &[Postgres],
        "DATETIME" | "JULIANDAY" | "PRINTF" | "STRFTIME" | "TYPEOF" => &[Sqlite],
        "CEIL" | "LPAD" | "MOD" | "NOW" | "RPAD" | "REGEXP_REPLACE" | "EXTRACT" | "POSITION" => {
            &[MySql, Postgres]
        }
        "CONVERT" | "DATEDIFF" | "RAND" | "YEAR" => &[MySql, SqlServer],
        "JSON_VALUE" => &[SqlServer, Postgres],
        "DATE" | "GROUP_CONCAT" | "IFNULL" | "INSTR" | "JSON_EXTRACT" => &[MySql, Sqlite],
        "IIF" => &[SqlServer, Sqlite],
        "RANDOM" => &[Postgres, Sqlite],
        "LENGTH" => &[MySql, Postgres, Sqlite],
        "FORMAT" => &[MySql, SqlServer, Sqlite],
        "STRING_AGG" => &[SqlServer, Postgres, Sqlite],
        "GREATEST" | "LEAST" => &[MySql, SqlServer, Postgres],
        _ => return true,
    };
    only.contains(&dialect)
}

/// Whether a keyword means anything in the dialect; those not listed are
/// everyone's. Highlighting still knows them all.
fn keyword_in(k: &str, dialect: Dialect) -> bool {
    use Dialect::*;
    let only: &[Dialect] = match k {
        "AUTO_INCREMENT" | "DELIMITER" | "DESCRIBE" | "DETERMINISTIC" | "ELSEIF" | "ENGINE"
        | "LEAVE" | "MODIFY" | "READS" | "REPEAT" | "SIGNAL" | "UNTIL" => &[MySql],
        "BREAK" | "CATCH" | "CLUSTERED" | "EXEC" | "GO" | "GOTO" | "NOCOUNT" | "NONCLUSTERED"
        | "OUTPUT" | "PRINT" | "PROC" | "RAISERROR" | "THROW" | "TOP" | "TRAN" | "TRY"
        | "WAITFOR" => &[SqlServer],
        "CONCURRENTLY" | "ELSIF" | "EXCEPTION" | "EXTENSION" | "ILIKE" | "IMMUTABLE"
        | "LANGUAGE" | "OWNER" | "PERFORM" | "REFRESH" | "SETOF" | "STABLE" | "VOLATILE" => {
            &[Postgres]
        }
        "ATTACH" | "AUTOINCREMENT" | "DETACH" | "PRAGMA" => &[Sqlite],
        "CONFLICT" | "DEFERRABLE" | "FILTER" | "ISNULL" | "MATERIALIZED" | "NOTNULL" | "NULLS"
        | "RAISE" | "STRICT" | "TEMP" | "VACUUM" => &[Postgres, Sqlite],
        "CALL" | "COMMENT" | "INOUT" | "LATERAL" | "OUT" | "SECURITY" | "SHOW" => {
            &[MySql, Postgres]
        }
        "DO" | "LIMIT" | "RECURSIVE" | "RENAME" | "RETURNING" => &[MySql, Postgres, Sqlite],
        "IDENTITY" => &[SqlServer, Postgres],
        "INSTEAD" => &[SqlServer, Postgres, Sqlite],
        "USE" => &[MySql, SqlServer],
        // server-side programming SQLite doesn't have
        "CLOSE" | "CONTINUE" | "CURSOR" | "DEALLOCATE" | "DECLARE" | "FETCH" | "FUNCTION"
        | "GRANT" | "LOOP" | "OPEN" | "PROCEDURE" | "RETURN" | "RETURNS" | "REVOKE" | "ROLE"
        | "SCHEMA" | "SEQUENCE" | "TRUNCATE" | "WHILE" => &[MySql, SqlServer, Postgres],
        // highlighted, but not something anyone types as a keyword
        "CTE" => &[],
        _ => return true,
    };
    only.contains(&dialect)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// An identifier, unquoted; `quoted` when it was in backticks or quotes.
    Ident {
        text: String,
        quoted: bool,
    },
    Punct(char),
    /// Strings, numbers and anything else that only takes up space.
    Other,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    range: Range<usize>,
    /// Parenthesis depth, and which parenthesis group it's in.
    depth: usize,
    group: usize,
}

/// Tokens, and the ranges of comments and strings, where nothing completes.
fn tokenize(sql: &str, dialect: Dialect) -> (Vec<Token>, Vec<Range<usize>>) {
    let mysql = dialect == Dialect::MySql;
    let postgres = dialect == Dialect::Postgres;
    // T-SQL names take # (temp tables) and @ (variables)
    let ident =
        |c: u8| is_ident_byte(c) || (dialect == Dialect::SqlServer && matches!(c, b'#' | b'@'));
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut dead = Vec::new();
    let mut i = 0;
    let mut depth = 0;
    // group ids: 0 is the top level; each "(" opens a new one
    let mut groups = vec![0usize];
    let mut next_group = 1;
    while i < b.len() {
        let c = b[i];
        let start = i;
        let tok = match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = sql[i..].find('\n').map_or(b.len(), |p| i + p);
                // a line comment runs to the end of the line, cursor included
                dead.push(start..i + 1);
                continue;
            }
            b'#' if mysql => {
                i = sql[i..].find('\n').map_or(b.len(), |p| i + p);
                dead.push(start..i + 1);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = sql[i + 2..]
                    .find("*/")
                    .map_or(b.len() + 1, |p| i + 2 + p + 2);
                dead.push(start..i);
                i = i.min(b.len());
                continue;
            }
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    if mysql && b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                dead.push(start..i);
                Tok::Other
            }
            b'$' if postgres && dollar_tag(sql, i).is_some() => {
                let tag = dollar_tag(sql, i).unwrap();
                i = sql[i + tag.len()..]
                    .find(tag)
                    .map_or(b.len() + 1, |p| i + tag.len() + p + tag.len());
                dead.push(start..i);
                i = i.min(b.len());
                continue;
            }
            b'`' | b'"' | b'[' if !(postgres && c == b'[') => {
                let close = if c == b'[' { b']' } else { c };
                let from = i + 1;
                i += 1;
                while i < b.len() && b[i] != close {
                    i += 1;
                }
                let text = sql[from..i.min(b.len())].to_string();
                i = (i + 1).min(b.len());
                Tok::Ident { text, quoted: true }
            }
            b'(' => {
                depth += 1;
                groups.push(next_group);
                next_group += 1;
                i += 1;
                Tok::Punct('(')
            }
            b')' => {
                i += 1;
                Tok::Punct(')')
            }
            _ if c.is_ascii_digit() => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                Tok::Other
            }
            _ if ident(c) => {
                while i < b.len() && ident(b[i]) {
                    i += 1;
                }
                Tok::Ident {
                    text: sql[start..i].to_string(),
                    quoted: false,
                }
            }
            _ => {
                // a whole char, so multi-byte punctuation stays on a boundary
                let ch = sql[i..].chars().next().unwrap();
                i += ch.len_utf8();
                Tok::Punct(ch)
            }
        };
        let token = Token {
            tok: tok.clone(),
            range: start..i,
            depth,
            group: *groups.last().unwrap(),
        };
        out.push(token);
        if tok == Tok::Punct(')') && depth > 0 {
            depth -= 1;
            groups.pop();
            // the ")" belongs to the group it closes
            out.last_mut().unwrap().depth = depth + 1;
        }
    }
    (out, dead)
}

/// `$tag$` or `$$` at `i`, unless the `$` is part of a name or `$1`.
fn dollar_tag(sql: &str, i: usize) -> Option<&str> {
    let b = sql.as_bytes();
    if i > 0 && is_ident_byte(b[i - 1]) {
        return None;
    }
    let len = b[i + 1..]
        .iter()
        .position(|&c| !(c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80))?;
    if b[i + 1 + len] != b'$' || b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    Some(&sql[i..i + len + 2])
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn word(t: &Token) -> Option<&str> {
    match &t.tok {
        Tok::Ident {
            text,
            quoted: false,
        } => Some(text),
        _ => None,
    }
}

fn is_kw(t: &Token, kw: &str) -> bool {
    word(t).is_some_and(|w| w.eq_ignore_ascii_case(kw))
}

fn is_keyword(w: &str) -> bool {
    let up = w.to_ascii_uppercase();
    KEYWORDS.binary_search(&up.as_str()).is_ok()
}

/// A table in scope: a real one, or a subquery with the columns its
/// select list names.
#[derive(Clone, Debug, PartialEq)]
struct Source {
    /// The names before the table's: database, or database and schema.
    qualifier: Vec<String>,
    name: String,
    alias: Option<String>,
    derived: Option<Vec<String>>,
}

impl Source {
    /// By its alias, or by its table name, which SQL rejects once there's
    /// an alias but people type anyway.
    fn called(&self, name: &str) -> bool {
        self.alias
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(name))
            || self.name.eq_ignore_ascii_case(name)
    }
}

/// Words that end a FROM list.
const FROM_ENDS: &[&str] = &[
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "ON",
    "USING",
    "SET",
    "WINDOW",
    "FOR",
    "INTO",
    "VALUES",
    "SELECT",
    "RETURNING",
];

/// Table references in one parenthesis group: after FROM, JOIN, UPDATE
/// and INTO, with their aliases.
fn sources(tokens: &[Token], group: usize) -> Vec<Source> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let starts = t.group == group
            && ["FROM", "JOIN", "UPDATE", "INTO"]
                .iter()
                .any(|k| is_kw(t, k));
        i += 1;
        if !starts {
            continue;
        }
        // a comma-separated list follows FROM
        loop {
            match tokens.get(i) {
                Some(t) if t.tok == Tok::Punct('(') => {
                    // derived table: (SELECT ...) alias
                    let inner = t.group;
                    let close = tokens[i + 1..]
                        .iter()
                        .position(|x| x.tok == Tok::Punct(')') && x.group == inner)
                        .map(|p| i + 1 + p);
                    let Some(close) = close else { break };
                    let columns = select_list(&tokens[i + 1..close], inner);
                    i = close + 1;
                    let alias = alias_at(tokens, &mut i);
                    if let Some(alias) = alias {
                        out.push(Source {
                            qualifier: vec![],
                            name: alias.clone(),
                            alias: Some(alias),
                            derived: Some(columns),
                        });
                    }
                }
                Some(t) if matches!(t.tok, Tok::Ident { .. }) && !is_clause(t) => {
                    let mut parts = vec![ident_text(t)];
                    let mut unfinished = false;
                    i += 1;
                    while tokens.get(i).is_some_and(|t| t.tok == Tok::Punct('.')) {
                        match tokens.get(i + 1) {
                            Some(t) if matches!(t.tok, Tok::Ident { .. }) => {
                                parts.push(ident_text(t));
                                i += 2;
                            }
                            _ => {
                                // `db.` still being typed
                                unfinished = true;
                                i += 1;
                                break;
                            }
                        }
                    }
                    if unfinished {
                        break;
                    }
                    let name = parts.pop().unwrap();
                    let alias = alias_at(tokens, &mut i);
                    out.push(Source {
                        qualifier: parts,
                        name,
                        alias,
                        derived: None,
                    });
                }
                _ => break,
            }
            if tokens
                .get(i)
                .is_some_and(|t| t.tok == Tok::Punct(',') && t.group == group)
            {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

fn is_clause(t: &Token) -> bool {
    FROM_ENDS.iter().any(|k| is_kw(t, k)) || is_kw(t, "JOIN")
}

fn ident_text(t: &Token) -> String {
    match &t.tok {
        Tok::Ident { text, .. } => text.clone(),
        _ => String::new(),
    }
}

/// `[AS] alias` after a table, if there is one.
fn alias_at(tokens: &[Token], i: &mut usize) -> Option<String> {
    if tokens.get(*i).is_some_and(|t| is_kw(t, "AS")) {
        *i += 1;
    }
    match tokens.get(*i).map(|t| &t.tok) {
        Some(Tok::Ident { quoted, text }) if *quoted || !is_keyword(text) => {
            *i += 1;
            Some(text.clone())
        }
        _ => None,
    }
}

/// Column names a subquery's select list gives its outer query: the
/// alias, or the last name in a plain column reference.
fn select_list(tokens: &[Token], group: usize) -> Vec<String> {
    let Some(start) = tokens
        .iter()
        .position(|t| t.group == group && is_kw(t, "SELECT"))
    else {
        return vec![];
    };
    let end = tokens[start..]
        .iter()
        .position(|t| t.group == group && is_kw(t, "FROM"))
        .map_or(tokens.len(), |p| start + p);
    let mut names = Vec::new();
    for item in tokens[start + 1..end].split(|t| t.tok == Tok::Punct(',') && t.group == group) {
        let top: Vec<&Token> = item.iter().filter(|t| t.group == group).collect();
        let name = match top.as_slice() {
            [.., a, b] if is_kw(a, "AS") => Some(ident_text(b)),
            [.., last] if matches!(last.tok, Tok::Ident { .. }) => Some(ident_text(last)),
            _ => None,
        };
        if let Some(n) = name.filter(|n| n != "*") {
            names.push(n);
        }
    }
    names
}

#[derive(Debug, PartialEq)]
enum Context {
    Tables,
    Procedures,
    Databases,
    Expression,
}

/// What kind of name goes at the cursor, from the clause it's in.
fn context(before: &[Token], group: usize) -> Context {
    let mut prev_significant: Option<&Token> = None;
    for t in before.iter().rev().filter(|t| t.group == group) {
        if prev_significant.is_none() {
            prev_significant = Some(t);
            // straight after one of these, a table name goes here
            if [
                "FROM", "JOIN", "UPDATE", "INTO", "TABLE", "DESCRIBE", "DESC",
            ]
            .iter()
            .any(|k| is_kw(t, k))
            {
                return Context::Tables;
            }
            if is_kw(t, "CALL") || is_kw(t, "EXEC") || is_kw(t, "EXECUTE") {
                return Context::Procedures;
            }
            if is_kw(t, "USE") {
                return Context::Databases;
            }
        }
        if t.tok == Tok::Punct(',') {
            continue;
        }
        // the clause the cursor is in decides what a comma continues
        if is_kw(t, "FROM") {
            return if prev_significant.is_some_and(|p| p.tok == Tok::Punct(',')) {
                Context::Tables
            } else {
                Context::Expression
            };
        }
        if [
            "SELECT", "WHERE", "ON", "SET", "BY", "HAVING", "VALUES", "JOIN",
        ]
        .iter()
        .any(|k| is_kw(t, k))
        {
            return Context::Expression;
        }
    }
    Context::Expression
}

/// Fuzzy match: every char of `pattern` in order, case-insensitive. Higher
/// is better; `None` is no match.
pub fn score(candidate: &str, pattern: &str) -> Option<i32> {
    if pattern.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let pat: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0;
    let mut ci = 0;
    let mut last = None;
    for &p in &pat {
        let found = cand[ci..].iter().position(|&c| c == p)?;
        let at = ci + found;
        score += match (last, at) {
            (_, 0) => 15,
            (Some(l), a) if a == l + 1 => 10,
            (_, a) if matches!(cand[a - 1], '_' | ' ' | '.') => 8,
            _ => 1,
        };
        score -= found as i32;
        last = Some(at);
        ci = at + 1;
    }
    if cand.starts_with(&pat) {
        score += 50;
    }
    // shorter names first when otherwise equal
    Some(score * 4 - cand.len() as i32)
}

/// Keywords follow the case the user is typing in.
fn keyword_case(keyword: &str, prefix: &str) -> String {
    let typed_lower =
        prefix.chars().any(|c| c.is_lowercase()) && !prefix.chars().any(|c| c.is_uppercase());
    if typed_lower {
        keyword.to_lowercase()
    } else {
        keyword.to_string()
    }
}

/// The object's schema when it isn't the one unqualified names find.
fn elsewhere(o: &SchemaObject, dialect: Dialect) -> Option<&str> {
    o.schema
        .as_deref()
        .filter(|s| Some(*s) != dialect.default_schema())
}

/// The schemas objects are in, each once.
fn schemas_of(objects: &[SchemaObject]) -> Vec<String> {
    let mut out: Vec<String> = objects.iter().filter_map(|o| o.schema.clone()).collect();
    out.sort();
    out.dedup();
    out
}

/// Names that need quoting to be used as typed.
fn quoted(name: &str, dialect: Dialect) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !is_keyword(name)
        // Postgres would fold it to lower case
        && !(dialect == Dialect::Postgres && name.chars().any(char::is_uppercase));
    match dialect {
        _ if plain => name.to_string(),
        Dialect::Postgres | Dialect::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
        Dialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
    }
}

pub fn complete(sql: &str, cursor: usize, schema: &dyn Schema) -> Completions {
    let cursor = cursor.min(sql.len());
    let dialect = schema.dialect();
    let (tokens, dead) = tokenize(sql, dialect);

    // the partly typed name: identifier chars just before the cursor
    let prefix_start = sql[..cursor]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
        .last()
        .map_or(cursor, |(i, _)| i);
    let prefix = &sql[prefix_start..cursor];
    let mut out = Completions {
        replace: prefix_start..cursor,
        ..Default::default()
    };

    // no completion inside strings or comments
    if dead.iter().any(|r| r.start < cursor && cursor < r.end) {
        return out;
    }

    let before: Vec<Token> = tokens
        .iter()
        .filter(|t| t.range.end <= prefix_start)
        .cloned()
        .collect();
    // the parenthesis group the cursor is in
    let mut stack = vec![0];
    for t in &before {
        match t.tok {
            Tok::Punct('(') => stack.push(t.group),
            Tok::Punct(')') if stack.len() > 1 => {
                stack.pop();
            }
            _ => {}
        }
    }
    let group = *stack.last().unwrap();

    // a qualifier, `name.` or `db.table.`, written right up to the prefix
    let mut qualifier: Vec<String> = Vec::new();
    let mut edge = prefix_start;
    let mut j = before.len();
    while j >= 2 {
        let (name, dot) = (&before[j - 2], &before[j - 1]);
        if dot.tok != Tok::Punct('.') || dot.range.end != edge || name.range.end != dot.range.start
        {
            break;
        }
        let Tok::Ident { text, .. } = &name.tok else {
            break;
        };
        qualifier.insert(0, text.clone());
        edge = name.range.start;
        j -= 2;
    }

    let current_db = schema.current_database().map(str::to_string);
    let mut scope = sources(&tokens, group);
    // the outer query's tables too, for correlated subqueries
    if group != 0 {
        scope.extend(sources(&tokens, 0));
    }

    let push = |out: &mut Completions, label: &str, kind: Kind, detail: Option<String>| {
        let insert = match kind {
            Kind::Keyword | Kind::Function => keyword_case(label, prefix),
            _ => quoted(label, dialect),
        };
        out.items.push(Candidate {
            label: label.to_string(),
            kind,
            detail,
            insert,
        });
    };
    // `qualify` writes the schema too where the name alone wouldn't find
    // the object; T-SQL's own functions always need it
    let push_object = |out: &mut Completions, o: &SchemaObject, qualify: bool| {
        let schema = o.schema.as_deref();
        let needs_schema = elsewhere(o, dialect).is_some()
            || (o.kind == Kind::Function && dialect == Dialect::SqlServer);
        let insert = match schema {
            Some(sc) if qualify && needs_schema => {
                format!("{}.{}", quoted(sc, dialect), quoted(&o.name, dialect))
            }
            _ => quoted(&o.name, dialect),
        };
        let detail = match (elsewhere(o, dialect), &o.detail) {
            (Some(sc), Some(d)) => Some(format!("{sc} · {d}")),
            (Some(sc), None) => Some(sc.to_string()),
            (None, d) => d.clone(),
        };
        out.items.push(Candidate {
            label: o.name.clone(),
            kind: o.kind,
            detail,
            insert,
        });
    };
    let objects_in = |out: &mut Completions, db: &str| -> Vec<SchemaObject> {
        match schema.objects(db) {
            Some(o) => o.to_vec(),
            None => {
                out.missing.push(Need::Objects(db.to_string()));
                vec![]
            }
        }
    };
    let is_database = |out: &mut Completions, name: &str| -> bool {
        match schema.databases() {
            Some(dbs) => dbs.iter().any(|d| d.eq_ignore_ascii_case(name)),
            None => {
                out.missing.push(Need::Databases);
                false
            }
        }
    };
    // the database and schema a qualifier before a table name points at
    let locate = |qualifier: &[String]| -> Option<(String, Option<String>)> {
        let current = || current_db.clone();
        Some(match (dialect.has_schemas(), qualifier) {
            (_, []) => (current()?, None),
            (true, [schema]) => (current()?, Some(schema.clone())),
            (true, [.., db, schema]) => (db.clone(), Some(schema.clone())),
            (false, [.., db]) => (db.clone(), None),
        })
    };
    let columns_of = |out: &mut Completions, s: &Source| -> Vec<SchemaColumn> {
        if let Some(cols) = &s.derived {
            return cols
                .iter()
                .map(|c| SchemaColumn {
                    name: c.clone(),
                    data_type: String::new(),
                })
                .collect();
        }
        let Some((db, sc)) = locate(&s.qualifier) else {
            return vec![];
        };
        match schema.columns(&db, sc.as_deref(), &s.name) {
            Some(c) => c.to_vec(),
            None => {
                out.missing.push(Need::Columns(db, sc, s.name.clone()));
                vec![]
            }
        }
    };
    let table = |qualifier: &[String], name: &str| Source {
        qualifier: qualifier.to_vec(),
        name: name.to_string(),
        alias: None,
        derived: None,
    };
    let in_schema = |o: &SchemaObject, name: &str| {
        o.schema
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(name))
    };

    // the clause decides what kind of name goes here, qualified or not
    let ctx = context(&before[..j], group);
    let fits = |o: &SchemaObject| match ctx {
        Context::Tables => matches!(o.kind, Kind::Table | Kind::View),
        Context::Procedures => o.kind == Kind::Procedure,
        _ => true,
    };
    if !qualifier.is_empty() {
        let q = qualifier.last().unwrap().clone();
        // a table or alias in scope: its columns
        let source = scope
            .iter()
            .find(|s| qualifier.len() == 1 && s.called(&q))
            .cloned();
        let push_columns = |out: &mut Completions, s: &Source| {
            for c in columns_of(out, s) {
                push(out, &c.name, Kind::Column, Some(c.data_type));
            }
        };
        match (source, dialect.has_schemas(), qualifier.len()) {
            (Some(source), ..) => push_columns(&mut out, &source),
            // schema.: its objects; db.: that database's schemas
            (None, true, 1) => {
                let here = match &current_db {
                    Some(db) => objects_in(&mut out, db),
                    None => vec![],
                };
                let found: Vec<&SchemaObject> = here.iter().filter(|o| in_schema(o, &q)).collect();
                if !found.is_empty() {
                    for o in found.into_iter().filter(|o| fits(o)) {
                        push_object(&mut out, o, false);
                    }
                } else if is_database(&mut out, &q) {
                    for sc in schemas_of(&objects_in(&mut out, &q)) {
                        push(&mut out, &sc, Kind::Schema, None);
                    }
                }
            }
            // db.schema. or schema.table.
            (None, true, 2) => {
                if is_database(&mut out, &qualifier[0]) {
                    for o in objects_in(&mut out, &qualifier[0]) {
                        if in_schema(&o, &q) && fits(&o) {
                            push_object(&mut out, &o, false);
                        }
                    }
                } else {
                    push_columns(&mut out, &table(&qualifier[..1], &q));
                }
            }
            (None, true, n) => push_columns(&mut out, &table(&qualifier[..n - 1], &q)),
            // db.: its objects
            (None, false, 1) => {
                if is_database(&mut out, &q) {
                    for o in objects_in(&mut out, &q) {
                        if fits(&o) {
                            push_object(&mut out, &o, false);
                        }
                    }
                }
            }
            // db.table.: that table's columns
            (None, false, 2) => push_columns(&mut out, &table(&qualifier[..1], &q)),
            _ => {}
        }
    } else {
        let tables_here = |out: &mut Completions| -> Vec<SchemaObject> {
            match &current_db {
                Some(db) => objects_in(out, db),
                None => vec![],
            }
        };
        match ctx {
            Context::Tables => {
                let here = tables_here(&mut out);
                for o in &here {
                    if matches!(o.kind, Kind::Table | Kind::View) {
                        push_object(&mut out, o, true);
                    }
                }
                for sc in schemas_of(&here) {
                    push(&mut out, &sc, Kind::Schema, None);
                }
                match schema.databases() {
                    Some(dbs) => {
                        for d in dbs {
                            push(&mut out, d, Kind::Database, None);
                        }
                    }
                    None => out.missing.push(Need::Databases),
                }
            }
            Context::Procedures => {
                for o in tables_here(&mut out) {
                    if o.kind == Kind::Procedure {
                        push_object(&mut out, &o, true);
                    }
                }
            }
            Context::Databases => match schema.databases() {
                Some(dbs) => {
                    for d in dbs {
                        push(&mut out, d, Kind::Database, None);
                    }
                }
                None => out.missing.push(Need::Databases),
            },
            Context::Expression => {
                for s in scope.clone() {
                    for c in columns_of(&mut out, &s) {
                        let detail = Some(match &s.alias {
                            Some(a) => format!("{} · {a}", c.data_type),
                            None => format!("{} · {}", c.data_type, s.name),
                        });
                        push(&mut out, &c.name, Kind::Column, detail);
                    }
                    let name = s.alias.clone().unwrap_or(s.name.clone());
                    push(&mut out, &name, Kind::Alias, None);
                }
                for o in tables_here(&mut out) {
                    if o.kind == Kind::Function {
                        push_object(&mut out, &o, true);
                    }
                }
                for f in FUNCTIONS.iter().filter(|f| function_in(f, dialect)) {
                    push(&mut out, f, Kind::Function, None);
                }
            }
        }
        if ctx != Context::Databases {
            for k in KEYWORDS.iter().filter(|k| keyword_in(k, dialect)) {
                push(&mut out, k, Kind::Keyword, None);
            }
        }
    }

    // a column named in two joined tables shows once per table, which is
    // fine; exact duplicates (same name, kind and detail) are not
    // names that start with what's typed first, then schema names before
    // functions and keywords, then the fuzzy score; ties keep schema order
    let lower = prefix.to_lowercase();
    let mut scored: Vec<(i32, Candidate)> = out
        .items
        .drain(..)
        .filter_map(|c| score(&c.label, prefix).map(|s| (s, c)))
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        let starts = |c: &Candidate| c.label.to_lowercase().starts_with(&lower);
        starts(b)
            .cmp(&starts(a))
            .then(a.kind.cmp(&b.kind))
            .then(sb.cmp(sa))
    });
    scored.dedup_by(|(_, a), (_, b)| a == b);
    // the typed word itself isn't a suggestion
    out.items = scored
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| !(c.label.eq_ignore_ascii_case(prefix) && c.kind == Kind::Keyword))
        .take(MAX_ITEMS)
        .collect();
    out.missing.sort_by_key(|n| format!("{n:?}"));
    out.missing.dedup();
    out
}

#[cfg(test)]
mod tests;

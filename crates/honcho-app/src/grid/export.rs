//! Turning cells into text for the clipboard and for files.

use std::cmp::Ordering;
use std::fmt::Write;

use honcho_db::{Column, DriverKind, Value};

/// Tab separated, the way spreadsheets paste it: no header, NULL as an
/// empty cell, and fields with tabs, newlines or quotes quoted.
pub fn tsv(rows: &[Vec<&Value>]) -> String {
    let mut out = String::new();
    for row in rows {
        for (i, v) in row.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            out.push_str(&quote_if(&plain(v), |c| {
                matches!(c, '\t' | '\n' | '\r' | '"')
            }));
        }
        out.push('\n');
    }
    out
}

/// RFC 4180 with a header line.
pub fn csv(columns: &[&Column], rows: &[Vec<&Value>]) -> String {
    let field = |s: &str| quote_if(s, |c| matches!(c, ',' | '\n' | '\r' | '"'));
    let mut out = String::new();
    let header: Vec<String> = columns.iter().map(|c| field(&c.name)).collect();
    out.push_str(&header.join(","));
    out.push_str("\r\n");
    for row in rows {
        let line: Vec<String> = row.iter().map(|v| field(&plain(v))).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}

/// An array of objects keyed by column name. Decimals stay exact: they're
/// written as JSON numbers straight from the server's text.
pub fn json(columns: &[&Column], rows: &[Vec<&Value>]) -> String {
    let mut out = String::from("[\n");
    for (r, row) in rows.iter().enumerate() {
        out.push_str("  {");
        for (i, (c, v)) in columns.iter().zip(row).enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{}: {}", json_string(&c.name), json_value(v));
        }
        out.push('}');
        if r + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    out
}

/// One INSERT per row. Without a table name from the server, `table_name`
/// stands in for it.
pub fn inserts(kind: DriverKind, columns: &[&Column], rows: &[Vec<&Value>]) -> String {
    let table = columns
        .iter()
        .find_map(|c| c.table.clone())
        .unwrap_or_else(|| "table_name".into());
    let names: Vec<String> = columns.iter().map(|c| ident(kind, &c.name)).collect();
    let mut out = String::new();
    for row in rows {
        let values: Vec<String> = row.iter().map(|v| literal(kind, v)).collect();
        let _ = writeln!(
            out,
            "INSERT INTO {} ({}) VALUES ({});",
            ident(kind, &table),
            names.join(", "),
            values.join(", ")
        );
    }
    out
}

/// Orders values the way a sort by column should: NULLs first, numbers by
/// value, everything else by its text.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    match (number(a), number(b)) {
        _ if a.is_null() || b.is_null() => b.is_null().cmp(&a.is_null()),
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.display(usize::MAX).cmp(&b.display(usize::MAX)),
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(d) => d.parse().ok(),
        _ => None,
    }
}

/// The cell as plain text, NULL as nothing.
fn plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        v => v.display(usize::MAX).into_owned(),
    }
}

fn quote_if(s: &str, special: impl Fn(char) -> bool) -> String {
    if s.chars().any(special) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Decimal(d) if serde_json::from_str::<serde_json::Number>(d).is_ok() => d.clone(),
        // a JSON column goes in as JSON, not as a string holding it
        Value::Json(j) if serde_json::from_str::<serde_json::Value>(j).is_ok() => j.clone(),
        v => json_string(&v.display(usize::MAX)),
    }
}

fn ident(kind: DriverKind, name: &str) -> String {
    match kind {
        DriverKind::MySql => format!("`{}`", name.replace('`', "``")),
        DriverKind::SqlServer => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn literal(kind: DriverKind, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Decimal(d) => d.clone(),
        Value::Bytes(b) => {
            let mut s = String::from("X'");
            for byte in b {
                let _ = write!(s, "{byte:02X}");
            }
            s.push('\'');
            s
        }
        v => {
            let text = v.display(usize::MAX);
            let mut escaped = text.replace('\'', "''");
            // MySQL also reads backslash escapes inside strings
            if kind == DriverKind::MySql {
                escaped = escaped.replace('\\', "\\\\");
            }
            format!("'{escaped}'")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<Column> {
        names.iter().map(|n| Column::new(*n, "text")).collect()
    }

    fn sample() -> Vec<Vec<Value>> {
        vec![
            vec![
                Value::Int(1),
                Value::Text("a\tb".into()),
                Value::Null,
                Value::Decimal("12.50".into()),
            ],
            vec![
                Value::Int(2),
                Value::Text("say \"hi\",\nok".into()),
                Value::Bytes(vec![0xde, 0xad]),
                Value::Decimal("0.10".into()),
            ],
        ]
    }

    fn refs(rows: &[Vec<Value>]) -> Vec<Vec<&Value>> {
        rows.iter().map(|r| r.iter().collect()).collect()
    }

    #[test]
    fn tsv_pastes_into_a_spreadsheet() {
        let rows = sample();
        assert_eq!(
            tsv(&refs(&rows)),
            "1\t\"a\tb\"\t\t12.50\n2\t\"say \"\"hi\"\",\nok\"\t0xDEAD\t0.10\n"
        );
    }

    #[test]
    fn csv_has_a_header() {
        let c = cols(&["id", "name, full", "x", "amount"]);
        let rows = sample();
        let out = csv(&c.iter().collect::<Vec<_>>(), &refs(&rows));
        assert_eq!(
            out,
            "id,\"name, full\",x,amount\r\n1,a\tb,,12.50\r\n2,\"say \"\"hi\"\",\nok\",0xDEAD,0.10\r\n"
        );
    }

    #[test]
    fn json_keeps_types_and_decimals() {
        let c = cols(&["id", "name", "x", "amount"]);
        let rows = sample();
        let out = json(&c.iter().collect::<Vec<_>>(), &refs(&rows));
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed[0]["id"], 1);
        assert_eq!(parsed[0]["x"], serde_json::Value::Null);
        assert_eq!(parsed[1]["name"], "say \"hi\",\nok");
        assert!(out.contains("\"amount\": 12.50"), "{out}");
        let j = json(
            &cols(&["doc"]).iter().collect::<Vec<_>>(),
            &[vec![&Value::Json(r#"{"a": [1]}"#.into())]],
        );
        let parsed: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed[0]["doc"]["a"][0], 1);
    }

    #[test]
    fn inserts_quote_for_the_dialect() {
        let mut c = cols(&["id", "name"]);
        c[0].table = Some("users".into());
        let rows = [vec![Value::Int(1), Value::Text("O'Brien \\ ไทย".into())]];
        let rows: Vec<Vec<&Value>> = rows.iter().map(|r| r.iter().collect()).collect();
        let refs: Vec<&Column> = c.iter().collect();
        assert_eq!(
            inserts(DriverKind::MySql, &refs, &rows),
            "INSERT INTO `users` (`id`, `name`) VALUES (1, 'O''Brien \\\\ ไทย');\n"
        );
        assert_eq!(
            inserts(DriverKind::Postgres, &refs, &rows),
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'O''Brien \\ ไทย');\n"
        );
        let no_table = cols(&["x"]);
        let out = inserts(
            DriverKind::MySql,
            &no_table.iter().collect::<Vec<_>>(),
            &[vec![&Value::Null]],
        );
        assert_eq!(out, "INSERT INTO `table_name` (`x`) VALUES (NULL);\n");
    }

    #[test]
    fn sorts_nulls_first_then_numbers_by_value() {
        let mut v = [
            Value::Decimal("10.5".into()),
            Value::Null,
            Value::Int(9),
            Value::UInt(100),
        ];
        v.sort_by(compare);
        assert_eq!(
            v,
            [
                Value::Null,
                Value::Int(9),
                Value::Decimal("10.5".into()),
                Value::UInt(100)
            ]
        );
        let mut t = [Value::Text("b".into()), Value::Text("a".into())];
        t.sort_by(compare);
        assert_eq!(t[0], Value::Text("a".into()));
    }
}

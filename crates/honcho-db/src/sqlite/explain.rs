//! `EXPLAIN QUERY PLAN` rows turned into a [`Plan`].
//!
//! SQLite gives each step as one line of text and nothing else: no costs,
//! no row estimates, and no way to run the statement and measure it.

use crate::{DbError, Plan, PlanFormat, PlanNode, Result};

/// One row of `EXPLAIN QUERY PLAN`: its id, its parent's id and the text.
pub type Row = (i64, i64, String);

pub fn parse(rows: &[Row]) -> Result<Plan> {
    let mut top = children(rows, 0);
    let root = match top.len() {
        // a plain INSERT ... VALUES has no plan
        0 => {
            return Err(DbError::Other(
                "SQLite has no plan for this statement.".into(),
            ))
        }
        1 => top.pop().unwrap(),
        _ => PlanNode {
            children: top,
            ..PlanNode::new("QUERY PLAN")
        },
    };
    Ok(Plan::new(
        root,
        false,
        Vec::new(),
        text(rows),
        PlanFormat::Text,
    ))
}

fn children(rows: &[Row], parent: i64) -> Vec<PlanNode> {
    rows.iter()
        .filter(|(id, p, _)| *p == parent && *id != parent)
        .map(|(id, _, detail)| PlanNode {
            children: children(rows, *id),
            ..node(detail)
        })
        .collect()
}

/// Splits `SEARCH o USING INDEX orders_user (user_id=?)` into the
/// operation, the table, the index and the condition.
fn node(detail: &str) -> PlanNode {
    let mut n = PlanNode::new(detail);
    let Some((verb, rest)) = detail
        .split_once(' ')
        .filter(|(v, _)| matches!(*v, "SCAN" | "SEARCH"))
    else {
        if detail.starts_with("USE TEMP B-TREE") {
            n.warnings
                .push("Sorts or groups in a temporary b-tree; an index could avoid it".into());
        }
        return n;
    };
    if rest == "CONSTANT ROW" {
        return n;
    }
    let (object, using) = match rest.split_once(" USING ") {
        Some((o, u)) => (o, Some(u)),
        None => (rest, None),
    };
    n.object = Some(object.to_string());
    n.operation = verb.to_string();
    let Some(using) = using else {
        n.full_scan = verb == "SCAN";
        return n;
    };
    let (using, condition) = match using.split_once(" (") {
        Some((u, c)) => (u, Some(c.trim_end_matches(')'))),
        None => (using, None),
    };
    n.condition = condition.map(str::to_string);
    // "COVERING INDEX name", "INTEGER PRIMARY KEY", "AUTOMATIC COVERING INDEX"
    let (kind, index) = match using.rsplit_once("INDEX ") {
        Some((kind, name)) => (format!("{kind}INDEX"), Some(name.to_string())),
        None => (using.to_string(), None),
    };
    n.operation = format!("{verb} USING {kind}");
    n.index = index.or_else(|| (kind == "INTEGER PRIMARY KEY").then(|| kind.clone()));
    if kind.starts_with("AUTOMATIC") {
        n.warnings.push(
            "SQLite builds a temporary index for this query; a real one would save that".into(),
        );
    }
    n
}

/// The tree the sqlite3 shell prints.
fn text(rows: &[Row]) -> String {
    fn walk(rows: &[Row], parent: i64, prefix: &str, out: &mut String) {
        let kids: Vec<&Row> = rows
            .iter()
            .filter(|(id, p, _)| *p == parent && *id != parent)
            .collect();
        for (i, (id, _, detail)) in kids.iter().enumerate() {
            let last = i + 1 == kids.len();
            out.push_str(prefix);
            out.push_str(if last { "`--" } else { "|--" });
            out.push_str(detail);
            out.push('\n');
            let deeper = format!("{prefix}{}", if last { "   " } else { "|  " });
            walk(rows, *id, &deeper, out);
        }
    }
    let mut out = String::from("QUERY PLAN\n");
    walk(rows, 0, "", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(r: &[(i64, i64, &str)]) -> Vec<Row> {
        r.iter().map(|(a, b, c)| (*a, *b, c.to_string())).collect()
    }

    #[test]
    fn a_join_with_sorts() {
        let plan = parse(&rows(&[
            (3, 0, "SCAN u"),
            (5, 0, "SEARCH o USING INDEX orders_user (user_id=?)"),
            (8, 0, "USE TEMP B-TREE FOR GROUP BY"),
        ]))
        .unwrap();
        assert_eq!(plan.root.operation, "QUERY PLAN");
        let [scan, search, temp] = &plan.root.children[..] else {
            panic!("{:?}", plan.root.children);
        };
        assert_eq!(scan.operation, "SCAN");
        assert_eq!(scan.object.as_deref(), Some("u"));
        assert!(scan.full_scan);
        assert_eq!(search.operation, "SEARCH USING INDEX");
        assert_eq!(search.index.as_deref(), Some("orders_user"));
        assert_eq!(search.condition.as_deref(), Some("user_id=?"));
        assert!(!search.full_scan);
        assert_eq!(temp.warnings.len(), 1);
        assert!(!plan.analyzed);
        assert_eq!(
            plan.raw,
            "QUERY PLAN\n|--SCAN u\n|--SEARCH o USING INDEX orders_user (user_id=?)\n`--USE TEMP B-TREE FOR GROUP BY\n"
        );
    }

    #[test]
    fn nested_subqueries() {
        let plan = parse(&rows(&[
            (1, 0, "COMPOUND QUERY"),
            (2, 1, "LEFT-MOST SUBQUERY"),
            (
                5,
                2,
                "SEARCH orders USING COVERING INDEX orders_user (user_id=?)",
            ),
            (7, 2, "LIST SUBQUERY 1"),
            (9, 7, "SCAN users"),
            (20, 1, "UNION ALL"),
            (22, 20, "SEARCH orders USING INTEGER PRIMARY KEY (rowid=?)"),
        ]))
        .unwrap();
        let compound = &plan.root;
        assert_eq!(compound.operation, "COMPOUND QUERY");
        let left = &compound.children[0];
        assert_eq!(left.children[0].operation, "SEARCH USING COVERING INDEX");
        assert_eq!(
            left.children[1].children[0].object.as_deref(),
            Some("users")
        );
        let pk = &compound.children[1].children[0];
        assert_eq!(pk.operation, "SEARCH USING INTEGER PRIMARY KEY");
        assert_eq!(pk.index.as_deref(), Some("INTEGER PRIMARY KEY"));
        assert_eq!(pk.condition.as_deref(), Some("rowid=?"));
        assert!(plan
            .raw
            .contains("   |  `--LIST SUBQUERY 1\n   |     `--SCAN users\n"));
    }

    #[test]
    fn odd_lines_stay_whole() {
        let n = node("SCAN CONSTANT ROW");
        assert_eq!(
            (n.operation.as_str(), n.object),
            ("SCAN CONSTANT ROW", None)
        );
        let n = node("SEARCH t USING AUTOMATIC COVERING INDEX (a=?)");
        assert_eq!(n.operation, "SEARCH USING AUTOMATIC COVERING INDEX");
        assert_eq!(n.index, None);
        assert_eq!(n.warnings.len(), 1);
        let n = node("SCAN t USING COVERING INDEX t_a");
        assert!(!n.full_scan);
        assert_eq!(n.condition, None);
    }

    #[test]
    fn no_rows_is_an_error() {
        assert!(parse(&[]).is_err());
    }
}

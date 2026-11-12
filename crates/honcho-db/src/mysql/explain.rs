//! Plans from MySQL and MariaDB.
//!
//! MySQL 8.3 and later describe a plan as a tree of operators
//! (`explain_json_format_version = 2`), with or without ANALYZE. MariaDB,
//! and MySQL for statements its tree can't show, use the older format: a
//! query block of tables, with sorts, temporary tables and subqueries
//! wrapped around them.

use serde_json::{Map, Value as Json};

use crate::{DbError, Plan, PlanFormat, PlanNode, Result};

/// MySQL's way of saying a statement has no tree plan.
pub const NO_TREE: &str = "<not executable by iterator executor>";

fn bad(what: &str) -> DbError {
    DbError::Other(format!("couldn't read the plan: {what}"))
}

fn pretty(json: &Json, raw: &str) -> String {
    serde_json::to_string_pretty(json).unwrap_or_else(|_| raw.to_string())
}

/// Scalars and lists as text; nested objects are flattened by the caller.
fn text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Array(items) => items.iter().map(text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

// MySQL writes some numbers as strings ("cost_info": {"read_cost": "0.25"})
fn number(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn parse_tree(raw: &str, analyze: bool) -> Result<Plan> {
    let json: Json = serde_json::from_str(raw).map_err(|e| bad(&e.to_string()))?;
    let root = json.as_object().ok_or_else(|| bad("no plan in it"))?;
    if root.get("operation").and_then(Json::as_str) == Some(NO_TREE) {
        return Err(bad("MySQL has no tree for this statement"));
    }
    Ok(Plan::new(
        tree_node(root, analyze),
        analyze,
        Vec::new(),
        pretty(&json, raw),
        PlanFormat::Json,
    ))
}

const TREE_TAKEN: &[&str] = &[
    "operation",
    "inputs",
    "query",
    "table_name",
    "alias",
    "schema_name",
    "index_name",
    "condition",
    "lookup_condition",
    "estimated_rows",
    "estimated_total_cost",
    "actual_rows",
    "actual_loops",
    "actual_last_row_ms",
];

fn tree_node(n: &Map<String, Json>, analyze: bool) -> PlanNode {
    let get = |k: &str| n.get(k).and_then(Json::as_str);
    let full = get("operation").unwrap_or("?");
    // "Filter: (o.amount > 10.00)": the condition has its own column
    let operation = full.split_once(": ").map_or(full, |(op, _)| op);
    let object = get("table_name").map(|t| match get("alias") {
        Some(a) if a != t => format!("{t} {a}"),
        _ => t.to_string(),
    });
    let loops = analyze.then(|| number(n.get("actual_loops"))).flatten();
    // rows and time are per loop
    let times = |v: Option<f64>| Some(v? * loops?);
    let mut details: Vec<(String, String)> = Vec::new();
    if operation != full {
        details.push(("operation".into(), full.into()));
    }
    details.extend(
        n.iter()
            .filter(|(k, v)| {
                !TREE_TAKEN.contains(&k.as_str())
                    && !k.starts_with("inputs")
                    && **v != Json::Bool(false)
            })
            .map(|(k, v)| (k.clone(), text(v))),
    );
    // inputs, and inputs_from_select_list for subqueries in the columns
    let children = n
        .iter()
        .filter(|(k, _)| k.starts_with("inputs"))
        .filter_map(|(_, v)| v.as_array())
        .flatten()
        .filter_map(Json::as_object)
        .map(|c| tree_node(c, analyze))
        .collect();
    PlanNode {
        operation: operation.to_string(),
        object,
        index: get("index_name").map(str::to_string),
        condition: get("condition")
            .or(get("lookup_condition"))
            .map(str::to_string),
        est_rows: number(n.get("estimated_rows")),
        est_cost: number(n.get("estimated_total_cost")),
        actual_rows: times(number(n.get("actual_rows"))),
        loops,
        actual_ms: times(number(n.get("actual_last_row_ms"))),
        full_scan: get("access_type") == Some("table"),
        details,
        warnings: Vec::new(),
        children,
    }
}

pub fn parse_classic(raw: &str, analyze: bool) -> Result<Plan> {
    let json: Json = serde_json::from_str(raw).map_err(|e| bad(&e.to_string()))?;
    let block = json
        .get("query_block")
        .ok_or_else(|| bad("no query block in it"))?;
    let root = classic("query_block", block, analyze)
        .pop()
        .ok_or_else(|| bad("an empty query block"))?;
    let mut totals = Vec::new();
    if let Some(ms) = number(json.pointer("/query_optimization/r_total_time_ms")) {
        totals.push(("Optimization time".into(), format!("{ms:.3} ms")));
    }
    Ok(Plan::new(
        root,
        analyze,
        totals,
        pretty(&json, raw),
        PlanFormat::Json,
    ))
}

/// Keys that hold more plan below them.
fn has_plan(v: &Json) -> bool {
    match v {
        Json::Object(o) => o.iter().any(|(k, v)| {
            matches!(k.as_str(), "table" | "query_block" | "nested_loop") || has_plan(v)
        }),
        Json::Array(items) => items.iter().any(has_plan),
        _ => false,
    }
}

fn label(key: &str, n: &Map<String, Json>) -> String {
    let named = match key {
        "table" => {
            return match n.get("access_type").and_then(Json::as_str) {
                Some("ALL") => "Full table scan",
                Some("index") => "Full index scan",
                Some("range") => "Index range scan",
                Some("ref") => "Index lookup",
                Some("eq_ref") => "Unique index lookup",
                Some("ref_or_null") => "Index lookup with nulls",
                Some("const" | "system") => "Single row",
                Some("index_merge") => "Index merge",
                Some("fulltext") => "Fulltext search",
                Some("unique_subquery" | "index_subquery") => "Subquery index lookup",
                Some(a) if a.starts_with("hash_") => "Hash join",
                Some(a) => return format!("Table ({a})"),
                None => "Table",
            }
            .to_string()
        }
        "query_block" => {
            return match n.get("select_id") {
                Some(id) => format!("Select #{id}"),
                None => "Select".into(),
            }
        }
        "filesort" | "ordering_operation" => "Sort",
        "temporary_table" => "Temporary table",
        "read_sorted_file" => "Read sorted file",
        "materialized" => "Materialize",
        "subqueries" | "attached_subqueries" => "Subqueries",
        "subquery_cache" => "Subquery cache",
        "union_result" => "Union",
        "duplicates_removal" => "Remove duplicates",
        "block-nl-join" => "Block nested loop join",
        "grouping_operation" => "Group",
        "window_functions_computation" => "Window functions",
        "nested_loop" => "Nested loop",
        other => {
            let words = other.replace(['_', '-'], " ");
            let mut chars = words.chars();
            return match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect(),
                None => String::new(),
            };
        }
    };
    named.to_string()
}

const CLASSIC_TAKEN: &[&str] = &[
    "table_name",
    "key",
    "attached_condition",
    "sort_key",
    "having_condition",
    "rows",
    "rows_examined_per_scan",
    "cost",
    "r_loops",
    "r_rows",
    "r_output_rows",
    "r_table_time_ms",
    "r_other_time_ms",
    "r_total_time_ms",
];

/// The nodes for one key of the classic format: usually one, none for
/// plain values, several for a list.
fn classic(key: &str, v: &Json, analyze: bool) -> Vec<PlanNode> {
    let n = match v {
        Json::Array(items) => {
            let children: Vec<PlanNode> = items
                .iter()
                .filter_map(Json::as_object)
                .flat_map(|o| o.iter().flat_map(|(k, v)| classic(k, v, analyze)))
                .collect();
            return match (key, children.len()) {
                // one table needs no loop around it
                ("nested_loop", 1) => children,
                ("nested_loop" | "subqueries" | "attached_subqueries", _) => {
                    let (est_cost, actual_ms) = rolled_up(None, None, false, &children);
                    vec![PlanNode {
                        est_cost,
                        actual_ms,
                        children,
                        ..PlanNode::new(label(key, &Map::new()))
                    }]
                }
                _ => children,
            };
        }
        Json::Object(n) => n,
        _ => return Vec::new(),
    };
    let get = |k: &str| n.get(k).and_then(Json::as_str);
    let is_table = key == "table";
    let r = |k: &str| analyze.then(|| number(n.get(k))).flatten();

    let mut details = Vec::new();
    let mut children = Vec::new();
    for (k, v) in n {
        if CLASSIC_TAKEN.contains(&k.as_str()) || *v == Json::Bool(false) {
            continue;
        }
        if has_plan(v) || matches!(k.as_str(), "table" | "query_block") {
            children.extend(classic(k, v, analyze));
        } else if let Json::Object(inner) = v {
            // cost_info, r_engine_stats and the like
            details.extend(inner.iter().map(|(ik, iv)| (format!("{k}.{ik}"), text(iv))));
        } else if analyze || !k.starts_with("r_") {
            details.push((k.clone(), text(v)));
        }
    }

    let loops = r("r_loops");
    let actual_rows = match (r("r_rows"), loops, r("r_output_rows")) {
        // r_rows is per loop
        (Some(rows), Some(l), _) => Some(rows * l),
        (_, _, out) => out,
    };
    let own_ms = match (r("r_table_time_ms"), r("r_other_time_ms")) {
        (None, None) => r("r_total_time_ms"),
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
    // only a table's cost is its own; the totals above are worked out below
    let own_cost = is_table
        .then(|| {
            number(n.get("cost")).or_else(|| {
                let read = number(n.get("cost_info")?.get("read_cost"));
                let eval = number(n.get("cost_info")?.get("eval_cost"));
                Some(read.unwrap_or(0.0) + eval.unwrap_or(0.0))
            })
        })
        .flatten();
    let (est_cost, actual_ms) = rolled_up(own_cost, own_ms, is_table, &children);

    vec![PlanNode {
        operation: label(key, n),
        object: get("table_name").map(str::to_string),
        index: get("key").map(str::to_string),
        condition: get("attached_condition")
            .or(get("sort_key"))
            .or(get("having_condition"))
            .map(str::to_string),
        est_rows: number(n.get("rows")).or(number(n.get("rows_examined_per_scan"))),
        est_cost,
        actual_rows,
        loops,
        actual_ms,
        full_scan: is_table && get("access_type") == Some("ALL"),
        details,
        warnings: Vec::new(),
        children,
    }]
}

/// Cost and time including the children: the classic format gives each
/// table its own and the wrappers around them none, or a total of their own.
fn rolled_up(
    own_cost: Option<f64>,
    own_ms: Option<f64>,
    is_table: bool,
    children: &[PlanNode],
) -> (Option<f64>, Option<f64>) {
    let sum = |f: fn(&PlanNode) -> Option<f64>| -> Option<f64> {
        children.iter().filter_map(f).reduce(|a, b| a + b)
    };
    let est_cost = match (own_cost, sum(|c| c.est_cost)) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
    let actual_ms = match (own_ms, sum(|c| c.actual_ms)) {
        (None, None) => None,
        // a table's time leaves out what's materialized below it
        (Some(own), Some(c)) if is_table => Some(own + c),
        (a, b) => Some(a.unwrap_or(0.0).max(b.unwrap_or(0.0))),
    };
    (est_cost, actual_ms)
}

/// The names a statement mentions, lower-cased: identifiers, quoted or not.
/// Strings and comments count too, which only errs towards finding more.
pub fn names(sql: &str) -> std::collections::HashSet<String> {
    sql.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Tables a plan reads or writes, by name without alias.
pub fn tables(node: &PlanNode, out: &mut Vec<String>) {
    if let Some(o) = &node.object {
        out.extend(o.split_whitespace().next().map(str::to_lowercase));
    }
    for c in &node.children {
        tables(c, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(n: &PlanNode, depth: usize, out: &mut Vec<String>) {
        let object = n
            .object
            .as_deref()
            .map(|o| format!(" [{o}]"))
            .unwrap_or_default();
        out.push(format!("{}{}{object}", "  ".repeat(depth), n.operation));
        n.children.iter().for_each(|c| walk(c, depth + 1, out));
    }

    fn outline(plan: &Plan) -> Vec<String> {
        let mut out = Vec::new();
        walk(&plan.root, 0, &mut out);
        out
    }

    #[test]
    fn a_mysql_tree() {
        let plan = parse_tree(include_str!("plans/mysql-join.json"), false).unwrap();
        assert_eq!(
            outline(&plan),
            [
                "Nested loop inner join",
                "  Sort",
                "    Filter",
                "      Table scan on o [orders o]",
                "  Single-row index lookup on u using PRIMARY (id=o.user_id) [users u]",
            ]
        );
        let filter = &plan.root.children[0].children[0];
        assert_eq!(filter.condition.as_deref(), Some("(o.amount > 10.00)"));
        assert!(filter.children[0].full_scan);
        assert_eq!(plan.root.children[1].index.as_deref(), Some("PRIMARY"));
        assert!(plan.root.est_cost.is_some());
        assert!(!plan.root.details.iter().any(|(k, _)| k == "query"));
    }

    #[test]
    fn an_analyzed_mysql_tree() {
        let plan = parse_tree(include_str!("plans/mysql-analyzed.json"), true).unwrap();
        let lookup = &plan.root.children[1];
        assert_eq!(lookup.loops, Some(2.0));
        assert_eq!(lookup.actual_rows, Some(2.0));
        assert!(plan.root.actual_ms.unwrap() > 0.0);
    }

    #[test]
    fn mysql_subqueries() {
        let plan = parse_tree(include_str!("plans/mysql-subqueries.json"), false).unwrap();
        let lines = outline(&plan);
        assert!(lines.iter().any(|l| l.contains("[orders x]")), "{lines:#?}");
    }

    #[test]
    fn names_in_a_statement() {
        let n = names("DELETE FROM `app`.`Log_Archive` l WHERE l.id IN (SELECT id FROM t2)");
        for w in ["app", "log_archive", "t2", "delete"] {
            assert!(n.contains(w), "{w}");
        }
    }

    #[test]
    fn no_tree_is_an_error() {
        let stub = format!(r#"{{"operation": "{NO_TREE}", "query_type": "update"}}"#);
        assert!(parse_tree(&stub, false).is_err());
    }

    #[test]
    fn a_mysql_classic_update() {
        let plan = parse_classic(include_str!("plans/mysql-classic-update.json"), false).unwrap();
        assert_eq!(outline(&plan), ["Select #1", "  Index range scan [orders]"]);
        let scan = &plan.root.children[0];
        assert_eq!(scan.index.as_deref(), Some("user_id"));
        assert_eq!(scan.est_rows, Some(1.0));
        assert!(scan.details.contains(&("update".into(), "true".into())));
    }

    #[test]
    fn a_mariadb_join() {
        let plan = parse_classic(include_str!("plans/mariadb-join.json"), false).unwrap();
        assert_eq!(
            outline(&plan),
            [
                "Select #1",
                "  Nested loop",
                "    Read sorted file",
                "      Sort",
                "        Full table scan [o]",
                "    Unique index lookup [u]",
            ]
        );
        let scan = &plan.root.children[0].children[0].children[0].children[0];
        assert!(scan.full_scan);
        assert_eq!(scan.condition.as_deref(), Some("o.amount > 10"));
        assert!(scan.actual_rows.is_none());
        // the select costs what its tables do
        let tables = 0.0113438 + 0.00350252;
        assert!((plan.root.est_cost.unwrap() - tables).abs() < 1e-9);
    }

    #[test]
    fn an_analyzed_mariadb_join() {
        let plan = parse_classic(include_str!("plans/mariadb-analyzed.json"), true).unwrap();
        let lookup = &plan.root.children[0].children[1];
        assert_eq!(lookup.loops, Some(2.0));
        assert_eq!(lookup.actual_rows, Some(2.0));
        assert!(lookup.actual_ms.unwrap() > 0.0);
        assert!(lookup
            .details
            .contains(&("r_engine_stats.pages_accessed".into(), "2".into())));
        assert_eq!(plan.totals[0].0, "Optimization time");
    }

    #[test]
    fn mariadb_subqueries() {
        let plan = parse_classic(include_str!("plans/mariadb-subqueries.json"), true).unwrap();
        let lines = outline(&plan);
        for table in ["[<subquery3>]", "[orders]", "[u]", "[x]"] {
            assert!(
                lines.iter().any(|l| l.ends_with(table)),
                "{table}: {lines:#?}"
            );
        }
        assert!(
            lines.iter().any(|l| l.trim() == "Subquery cache"),
            "{lines:#?}"
        );
    }
}

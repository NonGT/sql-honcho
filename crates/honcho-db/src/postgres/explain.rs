//! `EXPLAIN (FORMAT JSON)` output turned into a [`Plan`].

use serde_json::{Map, Value as Json};

use crate::{DbError, Plan, PlanFormat, PlanNode, Result};

/// Keys that become [`PlanNode`] fields, or that say nothing worth showing.
const TAKEN: &[&str] = &[
    "Node Type",
    "Plans",
    "Relation Name",
    "Schema",
    "Alias",
    "Index Name",
    "Function Name",
    "CTE Name",
    "Join Type",
    "Strategy",
    "Scan Direction",
    "Total Cost",
    "Plan Rows",
    "Actual Rows",
    "Actual Loops",
    "Actual Total Time",
];

const CONDITIONS: &[&str] = &[
    "Index Cond",
    "Hash Cond",
    "Merge Cond",
    "Recheck Cond",
    "Join Filter",
    "Filter",
];

pub fn sql(statement: &str, analyze: bool) -> String {
    let options = if analyze {
        "ANALYZE, BUFFERS, FORMAT JSON"
    } else {
        "FORMAT JSON"
    };
    format!("EXPLAIN ({options}) {statement}")
}

pub fn parse(raw: &str, analyze: bool) -> Result<Plan> {
    let bad = |what: &str| DbError::Other(format!("couldn't read the plan: {what}"));
    let json: Json = serde_json::from_str(raw).map_err(|e| bad(&e.to_string()))?;
    let top = json
        .get(0)
        .and_then(Json::as_object)
        .ok_or_else(|| bad("no plan in it"))?;
    let root = top
        .get("Plan")
        .and_then(Json::as_object)
        .ok_or_else(|| bad("no plan in it"))?;
    let mut totals = Vec::new();
    for key in ["Planning Time", "Execution Time"] {
        if let Some(ms) = top.get(key).and_then(Json::as_f64) {
            totals.push((key.to_string(), format!("{ms:.3} ms")));
        }
    }
    if let Some(triggers) = top.get("Triggers").and_then(Json::as_array) {
        for t in triggers {
            let name = t["Trigger Name"].as_str().unwrap_or("trigger");
            let ms = t["Time"].as_f64().unwrap_or(0.0);
            let calls = t["Calls"].as_f64().unwrap_or(0.0);
            totals.push((
                format!("Trigger {name}"),
                format!("{ms:.3} ms, {calls} calls"),
            ));
        }
    }
    let raw = serde_json::to_string_pretty(&json).unwrap_or_else(|_| raw.to_string());
    Ok(Plan::new(
        node(root, analyze),
        analyze,
        totals,
        raw,
        PlanFormat::Json,
    ))
}

fn node(n: &Map<String, Json>, analyze: bool) -> PlanNode {
    let text = |k: &str| n.get(k).and_then(Json::as_str).map(str::to_string);
    let num = |k: &str| n.get(k).and_then(Json::as_f64);
    let kind = text("Node Type").unwrap_or_else(|| "?".into());

    let loops = analyze.then(|| num("Actual Loops")).flatten();
    // the server gives rows and time per loop
    let times = |v: Option<f64>| match (v, loops) {
        (Some(v), Some(l)) => Some(v * l),
        _ => None,
    };
    let mut warnings = Vec::new();
    if text("Sort Space Type").as_deref() == Some("Disk") {
        warnings.push("Sort spilled to disk".into());
    }
    if loops == Some(0.0) {
        warnings.push("Never ran".into());
    }
    let children = n
        .get("Plans")
        .and_then(Json::as_array)
        .map(|plans| {
            plans
                .iter()
                .filter_map(Json::as_object)
                .map(|c| node(c, analyze))
                .collect()
        })
        .unwrap_or_default();

    PlanNode {
        operation: operation(&kind, n),
        object: object(n),
        index: text("Index Name"),
        condition: CONDITIONS.iter().find_map(|k| text(k)),
        est_rows: num("Plan Rows"),
        est_cost: num("Total Cost"),
        actual_rows: times(num("Actual Rows")),
        loops,
        actual_ms: times(num("Actual Total Time")),
        full_scan: kind == "Seq Scan",
        details: details(n),
        warnings,
        children,
    }
}

/// The name EXPLAIN's text format would print.
fn operation(kind: &str, n: &Map<String, Json>) -> String {
    let get = |k: &str| n.get(k).and_then(Json::as_str);
    let mut op = match (kind, get("Strategy")) {
        ("Aggregate", Some("Hashed")) => "HashAggregate".to_string(),
        ("Aggregate", Some("Sorted")) => "GroupAggregate".to_string(),
        ("SetOp", Some("Hashed")) => "HashSetOp".to_string(),
        _ => kind.to_string(),
    };
    match get("Join Type") {
        Some("Inner") | None => {}
        Some(join) if op.ends_with(" Join") => {
            op = format!("{} {join} Join", op.trim_end_matches(" Join"));
        }
        Some(join) => op = format!("{op} {join} Join"),
    }
    if get("Scan Direction") == Some("Backward") {
        op.push_str(" Backward");
    }
    op
}

fn object(n: &Map<String, Json>) -> Option<String> {
    let get = |k: &str| n.get(k).and_then(Json::as_str);
    let name = get("Relation Name")
        .or(get("Function Name"))
        .or(get("CTE Name"))?;
    let name = match get("Schema") {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    };
    Some(match get("Alias") {
        Some(a) if a != name => format!("{name} {a}"),
        _ => name,
    })
}

fn details(n: &Map<String, Json>) -> Vec<(String, String)> {
    n.iter()
        .filter(|(k, v)| {
            // BUFFERS adds a dozen counters per node, mostly zero
            let zero_blocks = k.ends_with(" Blocks") && v.as_f64() == Some(0.0);
            !TAKEN.contains(&k.as_str()) && !zero_blocks && **v != Json::Bool(false)
        })
        .map(|(k, v)| {
            let v = match v {
                Json::String(s) => s.clone(),
                Json::Array(items) => items
                    .iter()
                    .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_string))
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            (k.clone(), v)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_estimated_join() {
        let plan = parse(include_str!("plans/join.json"), false).unwrap();
        assert!(!plan.analyzed);
        let join = &plan.root;
        assert_eq!(join.operation, "Hash Join");
        assert_eq!(join.condition.as_deref(), Some("(o.user_id = u.id)"));
        assert_eq!(join.est_cost, Some(69.12));
        assert_eq!(join.actual_rows, None);
        let [orders, hash] = &join.children[..] else {
            panic!("two children: {:?}", join.children);
        };
        assert_eq!(orders.operation, "Seq Scan");
        assert_eq!(orders.object.as_deref(), Some("orders o"));
        assert_eq!(
            orders.condition.as_deref(),
            Some("(amount > '10'::numeric)")
        );
        assert!(orders.full_scan);
        assert_eq!(hash.children[0].object.as_deref(), Some("users u"));
        assert!(orders
            .details
            .contains(&("Parent Relationship".into(), "Outer".into())));
        assert!(!orders.details.iter().any(|(k, _)| k == "Parallel Aware"));
        assert!(plan.raw.contains("\"Hash Cond\""));
    }

    #[test]
    fn an_analyzed_sort() {
        let plan = parse(include_str!("plans/analyzed.json"), true).unwrap();
        assert!(plan.analyzed);
        let sort = &plan.root;
        assert_eq!(sort.operation, "Sort");
        assert_eq!(sort.loops, Some(1.0));
        assert!(sort.actual_ms.is_some());
        assert!(sort.details.contains(&("Sort Key".into(), "g.g".into())));
        assert!(!sort.details.iter().any(|(k, _)| k == "Local Read Blocks"));
        let join = &sort.children[0];
        assert_eq!(join.operation, "Hash Left Join");
        assert_eq!(join.actual_rows, Some(20000.0));
        let names: Vec<_> = plan.totals.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["Planning Time", "Execution Time"]);
    }

    #[test]
    fn operation_names_match_the_text_format() {
        let obj = |j: Json| j.as_object().unwrap().clone();
        let n = obj(serde_json::json!({"Strategy": "Hashed"}));
        assert_eq!(operation("Aggregate", &n), "HashAggregate");
        let n = obj(serde_json::json!({"Join Type": "Anti"}));
        assert_eq!(operation("Nested Loop", &n), "Nested Loop Anti Join");
        let n = obj(serde_json::json!({"Scan Direction": "Backward"}));
        assert_eq!(operation("Index Scan", &n), "Index Scan Backward");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse("not json", false).is_err());
        assert!(parse("[]", false).is_err());
    }
}

//! Showplan XML turned into a [`Plan`]. The same parser reads estimated
//! plans (SET SHOWPLAN_XML) and actual ones (SET STATISTICS XML), which
//! add a RunTimeInformation element to every operator.

use roxmltree::{Document, Node};

use crate::{DbError, Plan, PlanFormat, PlanNode, Result};

/// The column name SQL Server gives the plan in both modes.
pub const PLAN_COLUMN: &str = "Microsoft SQL Server 2005 XML Showplan";

const RELOP_TAKEN: &[&str] = &[
    "NodeId",
    "PhysicalOp",
    "LogicalOp",
    "EstimateRows",
    "EstimatedTotalSubtreeCost",
];

fn bad(what: &str) -> DbError {
    DbError::Other(format!("couldn't read the plan: {what}"))
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(name))
}

fn num(n: Node, attr: &str) -> Option<f64> {
    n.attribute(attr)?.parse().ok()
}

/// `[dbo].[orders]` as `dbo.orders`.
fn unbracket(s: &str) -> String {
    s.replace(['[', ']'], "")
}

/// Descendants of `n` without going into the operators below it.
fn own<'a, 'i>(n: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    fn walk<'a, 'i>(n: Node<'a, 'i>, out: &mut Vec<Node<'a, 'i>>) {
        for c in n.children().filter(Node::is_element) {
            if c.has_tag_name("RelOp") {
                continue;
            }
            out.push(c);
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    walk(n, &mut out);
    out.into_iter()
}

/// The operators directly below `n`, however deep their elements sit.
fn inputs<'a, 'i>(n: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    fn walk<'a, 'i>(n: Node<'a, 'i>, out: &mut Vec<Node<'a, 'i>>) {
        for c in n.children().filter(Node::is_element) {
            if c.has_tag_name("RelOp") {
                out.push(c);
            } else {
                walk(c, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(n, &mut out);
    out
}

#[cfg(test)]
pub fn parse(raw: &str, analyze: bool) -> Result<Plan> {
    parse_all(&[raw.to_string()], analyze)
}

/// Plans from one run: a procedure call sends one per statement inside it.
pub fn parse_all(raws: &[String], analyze: bool) -> Result<Plan> {
    let mut nodes = Vec::new();
    let mut totals = Vec::new();
    for raw in raws {
        let doc = Document::parse(raw).map_err(|e| bad(&e.to_string()))?;
        let statements: Vec<Node> = doc
            .descendants()
            .filter(|n| n.has_tag_name("StmtSimple") && child(*n, "QueryPlan").is_some())
            .collect();
        if totals.is_empty() {
            totals = statement_totals(&statements);
        }
        nodes.extend(statements.iter().map(|s| statement(*s)));
    }
    let root = match nodes.len() {
        // trivial statements, such as SELECT 1, get none
        0 => {
            return Err(DbError::Other(
                "SQL Server has no plan for this statement.".into(),
            ))
        }
        1 => nodes.pop().unwrap(),
        _ => {
            let est_cost = nodes.iter().filter_map(|n| n.est_cost).reduce(|a, b| a + b);
            let actual_ms = nodes
                .iter()
                .filter_map(|n| n.actual_ms)
                .reduce(|a, b| a + b);
            PlanNode {
                est_cost,
                actual_ms,
                children: nodes,
                ..PlanNode::new("Batch")
            }
        }
    };
    Ok(Plan::new(
        root,
        analyze,
        totals,
        raws.join("\n"),
        PlanFormat::Xml,
    ))
}

fn statement_totals(statements: &[Node]) -> Vec<(String, String)> {
    let mut totals = Vec::new();
    if let Some(qp) = statements.first().and_then(|s| child(*s, "QueryPlan")) {
        if let Some(ms) = num(qp, "CompileTime") {
            totals.push(("Compile time".into(), format!("{ms} ms")));
        }
        if let Some(stats) = child(qp, "QueryTimeStats") {
            for (attr, name) in [("ElapsedTime", "Elapsed time"), ("CpuTime", "CPU time")] {
                if let Some(ms) = num(stats, attr) {
                    totals.push((name.into(), format!("{ms} ms")));
                }
            }
        }
    }
    totals
}

/// A statement, with its top operator below it, like SSMS shows it.
fn statement(s: Node) -> PlanNode {
    let qp = child(s, "QueryPlan").expect("checked by the caller");
    let top: Vec<PlanNode> = child(qp, "RelOp").map(relop).into_iter().collect();
    let mut warnings = plan_warnings(qp);
    warnings.extend(missing_indexes(qp));
    let details = [
        "StatementText",
        "StatementOptmLevel",
        "CardinalityEstimationModelVersion",
    ]
    .iter()
    .filter_map(|a| Some((a.to_string(), s.attribute(*a)?.to_string())))
    .chain(
        ["DegreeOfParallelism", "MemoryGrant"]
            .iter()
            .filter_map(|a| Some((a.to_string(), qp.attribute(*a)?.to_string()))),
    )
    .collect();
    PlanNode {
        operation: s
            .attribute("StatementType")
            .unwrap_or("Statement")
            .to_string(),
        est_rows: num(s, "StatementEstRows"),
        est_cost: num(s, "StatementSubTreeCost"),
        actual_rows: top.first().and_then(|t| t.actual_rows),
        loops: top.first().and_then(|t| t.loops),
        actual_ms: top.first().and_then(|t| t.actual_ms),
        details,
        warnings,
        children: top,
        ..Default::default()
    }
}

fn relop(r: Node) -> PlanNode {
    let physical = r.attribute("PhysicalOp").unwrap_or("?");
    let operation = match r.attribute("LogicalOp") {
        Some(l) if l != physical => format!("{physical} ({l})"),
        _ => physical.to_string(),
    };
    let mine: Vec<Node> = own(r).collect();
    let object = mine.iter().find(|n| n.has_tag_name("Object"));
    let object_name = object.and_then(|o| {
        let table = o.attribute("Table")?;
        let name = match o.attribute("Schema") {
            Some(s) => format!("{}.{}", unbracket(s), unbracket(table)),
            None => unbracket(table),
        };
        Some(match o.attribute("Alias") {
            Some(a) if unbracket(a) != unbracket(table) => format!("{name} {}", unbracket(a)),
            _ => name,
        })
    });

    // counters come per thread: rows and executions add up, elapsed time
    // is the slowest thread's
    let counters: Vec<Node> = child(r, "RunTimeInformation")
        .map(|ri| {
            ri.children()
                .filter(|c| c.has_tag_name("RunTimeCountersPerThread"))
                .collect()
        })
        .unwrap_or_default();
    let sum = |attr: &str| -> Option<f64> {
        counters
            .iter()
            .filter_map(|c| num(*c, attr))
            .reduce(|a, b| a + b)
    };
    let actual_ms = counters
        .iter()
        .filter_map(|c| num(*c, "ActualElapsedms"))
        .reduce(f64::max);

    let mut details: Vec<(String, String)> = r
        .attributes()
        .filter(|a| !RELOP_TAKEN.contains(&a.name()))
        .map(|a| (a.name().to_string(), a.value().to_string()))
        .collect();
    for (attr, name) in [
        ("ActualRowsRead", "ActualRowsRead"),
        ("ActualLogicalReads", "ActualLogicalReads"),
        ("ActualCPUms", "ActualCPUms"),
    ] {
        if let Some(v) = sum(attr) {
            details.push((name.into(), v.to_string()));
        }
    }
    let output: Vec<String> = child(r, "OutputList")
        .into_iter()
        .flat_map(|o| o.children().filter(|c| c.has_tag_name("ColumnReference")))
        .map(column)
        .collect();
    if !output.is_empty() {
        details.push(("Output".into(), output.join(", ")));
    }

    PlanNode {
        operation,
        object: object_name,
        index: object.and_then(|o| o.attribute("Index")).map(unbracket),
        condition: condition(&mine),
        est_rows: num(r, "EstimateRows"),
        est_cost: num(r, "EstimatedTotalSubtreeCost"),
        actual_rows: sum("ActualRows"),
        loops: sum("ActualExecutions"),
        actual_ms,
        full_scan: matches!(physical, "Table Scan" | "Clustered Index Scan"),
        details,
        warnings: child(r, "Warnings").map(warnings).unwrap_or_default(),
        children: inputs(r).into_iter().map(relop).collect(),
    }
}

/// `[app].[dbo].[orders].[amount] as [o].[amount]` reads better as `o.amount`.
fn column(c: Node) -> String {
    let name = c.attribute("Column").unwrap_or("?");
    match c.attribute("Alias").or(c.attribute("Table")) {
        Some(t) => format!("{}.{name}", unbracket(t)),
        None => name.to_string(),
    }
}

fn condition(mine: &[Node]) -> Option<String> {
    let seeks: Vec<String> = mine
        .iter()
        .filter(|n| n.has_tag_name("Prefix") || n.has_tag_name("StartRange"))
        .filter_map(|p| {
            let col = child(*p, "RangeColumns")?
                .children()
                .find(|c| c.has_tag_name("ColumnReference"))?;
            let expr = child(*p, "RangeExpressions")?
                .children()
                .find(|c| c.has_tag_name("ScalarOperator"))?
                .attribute("ScalarString")?;
            let op = match p.attribute("ScanType") {
                Some("GT") => ">",
                Some("GE") => ">=",
                Some("LT") => "<",
                Some("LE") => "<=",
                _ => "=",
            };
            Some(format!("{} {op} {}", column(col), tidy(expr)))
        })
        .collect();
    if !seeks.is_empty() {
        return Some(seeks.join(" AND "));
    }
    mine.iter()
        .filter(|n| n.has_tag_name("Predicate") || n.has_tag_name("ProbeResidual"))
        .find_map(|p| {
            p.children()
                .find(|c| c.has_tag_name("ScalarOperator"))?
                .attribute("ScalarString")
                .map(tidy)
        })
}

/// Drops the database.schema.table that SQL Server puts before every
/// aliased column: `[app].[dbo].[orders].[amount] as [o].[amount]` → `o.amount`.
fn tidy(expr: &str) -> String {
    let mut out = String::new();
    let mut rest = expr;
    while let Some(at) = rest.find(" as [") {
        // the part before " as " is a bracketed column path; drop it
        let before = &rest[..at];
        let start = before
            .rfind(|c: char| {
                !(c == '['
                    || c == ']'
                    || c == '.'
                    || c.is_alphanumeric()
                    || c == '_'
                    || c == '#'
                    || c == '@'
                    || c == '$')
            })
            .map_or(0, |i| i + 1);
        out.push_str(&before[..start]);
        rest = &rest[at + 4..];
    }
    out.push_str(rest);
    unbracket(&out)
}

fn warnings(w: Node) -> Vec<String> {
    w.children()
        .filter(Node::is_element)
        .map(|c| match c.tag_name().name() {
            "SpillToTempDb" => "Spilled to tempdb".to_string(),
            "NoJoinPredicate" => "No join predicate".to_string(),
            "ColumnsWithNoStatistics" => "Columns without statistics".to_string(),
            "PlanAffectingConvert" => format!(
                "Conversion may affect the plan: {}",
                c.attribute("Expression").map(tidy).unwrap_or_default()
            ),
            "MemoryGrantWarning" => "Memory grant warning".to_string(),
            other => other.to_string(),
        })
        .collect()
}

fn plan_warnings(qp: Node) -> Vec<String> {
    child(qp, "Warnings").map(warnings).unwrap_or_default()
}

fn missing_indexes(qp: Node) -> Vec<String> {
    let Some(groups) = child(qp, "MissingIndexes") else {
        return Vec::new();
    };
    groups
        .children()
        .filter(|g| g.has_tag_name("MissingIndexGroup"))
        .flat_map(|g| {
            let impact = num(g, "Impact").unwrap_or(0.0);
            g.children()
                .filter(|i| i.has_tag_name("MissingIndex"))
                .map(move |i| {
                    let table = format!(
                        "{}.{}",
                        unbracket(i.attribute("Schema").unwrap_or("")),
                        unbracket(i.attribute("Table").unwrap_or(""))
                    );
                    let cols = |usage: &[&str]| -> Vec<String> {
                        i.children()
                            .filter(|cg| {
                                cg.has_tag_name("ColumnGroup")
                                    && usage.contains(&cg.attribute("Usage").unwrap_or(""))
                            })
                            .flat_map(|cg| cg.children().filter(|c| c.has_tag_name("Column")))
                            .filter_map(|c| c.attribute("Name").map(unbracket))
                            .collect()
                    };
                    let key = cols(&["EQUALITY", "INEQUALITY"]).join(", ");
                    let include = cols(&["INCLUDE"]);
                    let include = if include.is_empty() {
                        String::new()
                    } else {
                        format!(" including {}", include.join(", "))
                    };
                    format!("Missing index on {table} ({key}){include}, {impact:.0}% better")
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(n: &PlanNode, depth: usize, out: &mut Vec<String>) {
        let object = n
            .object
            .as_deref()
            .map(|o| format!(" [{o}]"))
            .unwrap_or_default();
        out.push(format!("{}{}{object}", "  ".repeat(depth), n.operation));
        n.children.iter().for_each(|c| outline(c, depth + 1, out));
    }

    fn lines(plan: &Plan) -> Vec<String> {
        let mut out = Vec::new();
        outline(&plan.root, 0, &mut out);
        out
    }

    #[test]
    fn an_estimated_join() {
        let plan = parse(include_str!("plans/join.sqlplan"), false).unwrap();
        assert_eq!(
            lines(&plan),
            [
                "SELECT",
                "  Nested Loops (Inner Join)",
                "    Sort",
                "      Clustered Index Scan [dbo.orders o]",
                "    Clustered Index Seek [dbo.users u]",
            ]
        );
        let seek = &plan.root.children[0].children[1];
        assert_eq!(seek.condition.as_deref(), Some("u.id = o.user_id"));
        assert!(seek.index.as_deref().unwrap().starts_with("PK__users"));
        let scan = &plan.root.children[0].children[0].children[0];
        assert_eq!(scan.condition.as_deref(), Some("o.amount>(10.00)"));
        assert!(scan.full_scan);
        assert_eq!(scan.actual_rows, None);
        assert_eq!(plan.format, PlanFormat::Xml);
        assert!(plan.totals.iter().any(|(k, _)| k == "Compile time"));
    }

    #[test]
    fn an_actual_plan() {
        let plan = parse(include_str!("plans/analyzed.sqlplan"), true).unwrap();
        let seek = &plan.root.children[0].children[1];
        assert_eq!(seek.loops, Some(2.0));
        assert_eq!(seek.actual_rows, Some(2.0));
        assert!(seek.actual_ms.is_some());
        assert_eq!(plan.root.actual_rows, Some(2.0));
        assert!(plan.totals.iter().any(|(k, _)| k == "Elapsed time"));
    }

    #[test]
    fn an_update() {
        let plan = parse(include_str!("plans/update.sqlplan"), false).unwrap();
        let lines = lines(&plan);
        assert_eq!(lines[0], "UPDATE");
        assert!(lines[1].contains("Clustered Index Update"), "{lines:#?}");
    }

    #[test]
    fn warnings_and_missing_indexes() {
        let xml = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan">
          <BatchSequence><Batch><Statements>
          <StmtSimple StatementType="SELECT" StatementSubTreeCost="1">
            <QueryPlan>
              <MissingIndexes><MissingIndexGroup Impact="87.5">
                <MissingIndex Database="[app]" Schema="[dbo]" Table="[orders]">
                  <ColumnGroup Usage="EQUALITY"><Column Name="[user_id]"/></ColumnGroup>
                  <ColumnGroup Usage="INCLUDE"><Column Name="[amount]"/></ColumnGroup>
                </MissingIndex>
              </MissingIndexGroup></MissingIndexes>
              <RelOp NodeId="0" PhysicalOp="Sort" LogicalOp="Sort" EstimateRows="1"
                     EstimatedTotalSubtreeCost="1">
                <Warnings><SpillToTempDb SpillLevel="1"/><NoJoinPredicate/></Warnings>
              </RelOp>
            </QueryPlan>
          </StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;
        let plan = parse(xml, false).unwrap();
        assert_eq!(
            plan.root.warnings,
            ["Missing index on dbo.orders (user_id) including amount, 88% better"]
        );
        assert_eq!(
            plan.root.children[0].warnings,
            ["Spilled to tempdb", "No join predicate"]
        );
    }

    #[test]
    fn tidy_drops_column_paths() {
        assert_eq!(
            tidy("[app].[dbo].[orders].[amount] as [o].[amount]>(10.00)"),
            "o.amount>(10.00)"
        );
        assert_eq!(
            tidy("[app].[dbo].[orders].[user_id] as [o].[user_id]=[app].[dbo].[users].[id] as [u].[id]"),
            "o.user_id=u.id"
        );
        assert_eq!(tidy("[@p1]"), "@p1");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse("<nope/>", false).is_err());
        assert!(parse("not xml", false).is_err());
    }
}

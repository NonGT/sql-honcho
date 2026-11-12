//! Query plans, in one shape whichever server explained the query.

/// Estimates this far off, either way, get flagged.
const MISESTIMATE: f64 = 10.0;
/// Below this many rows neither a full scan nor a bad estimate matters.
const BIG: f64 = 10_000.0;
const MISESTIMATE_ROWS: f64 = 100.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub root: PlanNode,
    /// The statement ran, so the nodes have actual figures.
    pub analyzed: bool,
    /// Figures for the whole statement, such as planning time.
    pub totals: Vec<(String, String)>,
    /// What the server sent, to copy out or save.
    pub raw: String,
    pub format: PlanFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanFormat {
    Json,
    Xml,
    Text,
}

impl PlanFormat {
    pub fn extension(self) -> &'static str {
        match self {
            PlanFormat::Json => "json",
            // what SSMS opens
            PlanFormat::Xml => "sqlplan",
            PlanFormat::Text => "txt",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanNode {
    pub operation: String,
    pub object: Option<String>,
    pub index: Option<String>,
    pub condition: Option<String>,
    /// Per execution of the node.
    pub est_rows: Option<f64>,
    /// Including the children, in the server's own unit.
    pub est_cost: Option<f64>,
    /// Over all executions.
    pub actual_rows: Option<f64>,
    /// How many times the node ran; zero when it never did.
    pub loops: Option<f64>,
    /// Including the children, over all executions.
    pub actual_ms: Option<f64>,
    /// Reads every row of `object`.
    pub full_scan: bool,
    /// Everything else the server said about the node, in its order.
    pub details: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub children: Vec<PlanNode>,
}

impl Plan {
    /// Adds the warnings the drivers don't work out themselves.
    pub fn new(
        mut root: PlanNode,
        analyzed: bool,
        totals: Vec<(String, String)>,
        raw: String,
        format: PlanFormat,
    ) -> Self {
        root.flag();
        Self {
            root,
            analyzed,
            totals,
            raw,
            format,
        }
    }

    /// The node's own share of the statement: of the time when it ran,
    /// of the estimated cost otherwise.
    pub fn share(&self, node: &PlanNode) -> Option<f64> {
        let (own, total) = if self.analyzed {
            (node.own_ms()?, self.root.actual_ms?)
        } else {
            (node.own_cost()?, self.root.est_cost?)
        };
        (total > 0.0).then(|| (own / total).clamp(0.0, 1.0))
    }
}

impl PlanNode {
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            ..Default::default()
        }
    }

    pub fn own_cost(&self) -> Option<f64> {
        let children: f64 = self.children.iter().filter_map(|c| c.est_cost).sum();
        Some((self.est_cost? - children).max(0.0))
    }

    pub fn own_ms(&self) -> Option<f64> {
        let children: f64 = self.children.iter().filter_map(|c| c.actual_ms).sum();
        Some((self.actual_ms? - children).max(0.0))
    }

    /// Actual rows per execution, to set against the estimate.
    pub fn rows_per_loop(&self) -> Option<f64> {
        match self.loops {
            Some(l) if l > 0.0 => Some(self.actual_rows? / l),
            Some(_) => None,
            None => self.actual_rows,
        }
    }

    fn flag(&mut self) {
        if let (Some(est), Some(actual)) = (self.est_rows, self.rows_per_loop()) {
            let (low, high) = if est < actual {
                (est, actual)
            } else {
                (actual, est)
            };
            if high >= MISESTIMATE_ROWS && high / low.max(1.0) >= MISESTIMATE {
                self.warnings.push(format!(
                    "Estimated {} rows, got {}",
                    count(est),
                    count(actual)
                ));
            }
        }
        if self.full_scan {
            let rows = self.rows_per_loop().or(self.est_rows).unwrap_or(0.0);
            if rows >= BIG {
                let what = self.object.as_deref().unwrap_or("the table");
                self.warnings
                    .push(format!("Reads all of {what} ({} rows)", count(rows)));
            }
        }
        for child in &mut self.children {
            child.flag();
        }
    }
}

/// A row count with thousands separators.
pub fn count(n: f64) -> String {
    let digits = format!("{:.0}", n.max(0.0));
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(rows: f64, cost: f64) -> PlanNode {
        PlanNode {
            object: Some("orders".into()),
            est_rows: Some(rows),
            est_cost: Some(cost),
            full_scan: true,
            ..PlanNode::new("Seq Scan")
        }
    }

    #[test]
    fn counts_have_separators() {
        assert_eq!(count(0.0), "0");
        assert_eq!(count(999.0), "999");
        assert_eq!(count(1234.4), "1,234");
        assert_eq!(count(1_000_000.0), "1,000,000");
    }

    #[test]
    fn share_is_the_nodes_own_part() {
        let root = PlanNode {
            est_cost: Some(100.0),
            children: vec![scan(10.0, 60.0), scan(10.0, 20.0)],
            ..PlanNode::new("Hash Join")
        };
        let plan = Plan::new(root, false, vec![], String::new(), PlanFormat::Json);
        assert_eq!(plan.share(&plan.root), Some(0.2));
        assert_eq!(plan.share(&plan.root.children[0]), Some(0.6));
    }

    #[test]
    fn big_full_scans_and_bad_estimates_are_flagged() {
        let mut off = scan(50.0, 1.0);
        off.actual_rows = Some(20_000.0);
        off.loops = Some(2.0);
        let root = PlanNode {
            children: vec![scan(20_000.0, 1.0), scan(500.0, 1.0), off],
            ..PlanNode::new("Append")
        };
        let plan = Plan::new(root, true, vec![], String::new(), PlanFormat::Json);
        let w: Vec<_> = plan.root.children.iter().map(|c| &c.warnings).collect();
        assert_eq!(*w[0], ["Reads all of orders (20,000 rows)"]);
        assert!(w[1].is_empty());
        assert_eq!(
            *w[2],
            [
                "Estimated 50 rows, got 10,000",
                "Reads all of orders (10,000 rows)"
            ]
        );
    }

    #[test]
    fn a_node_that_never_ran_is_not_misestimated() {
        let mut never = scan(5000.0, 1.0);
        never.full_scan = false;
        never.actual_rows = Some(0.0);
        never.loops = Some(0.0);
        let plan = Plan::new(never, true, vec![], String::new(), PlanFormat::Json);
        assert!(plan.root.warnings.is_empty());
    }
}

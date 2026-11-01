use std::collections::HashMap;
use std::time::Instant;

use super::*;

#[derive(Default)]
struct TestSchema {
    current: Option<String>,
    databases: Option<Vec<String>>,
    objects: HashMap<String, Vec<SchemaObject>>,
    columns: HashMap<(String, String), Vec<SchemaColumn>>,
}

impl Schema for TestSchema {
    fn current_database(&self) -> Option<&str> {
        self.current.as_deref()
    }
    fn databases(&self) -> Option<&[String]> {
        self.databases.as_deref()
    }
    fn objects(&self, database: &str) -> Option<&[SchemaObject]> {
        self.objects.get(database).map(Vec::as_slice)
    }
    fn columns(&self, database: &str, table: &str) -> Option<&[SchemaColumn]> {
        self.columns
            .get(&(database.to_string(), table.to_string()))
            .map(Vec::as_slice)
    }
}

fn obj(name: &str, kind: Kind, detail: Option<&str>) -> SchemaObject {
    SchemaObject {
        name: name.into(),
        kind,
        detail: detail.map(Into::into),
    }
}

fn shop() -> TestSchema {
    let mut s = TestSchema {
        current: Some("shop".into()),
        databases: Some(vec!["shop".into(), "analytics".into()]),
        ..Default::default()
    };
    s.objects.insert(
        "shop".into(),
        vec![
            obj("orders", Kind::Table, None),
            obj("users", Kind::Table, None),
            obj("order items", Kind::Table, None),
            obj("active_users", Kind::View, None),
            obj("refresh_stats", Kind::Procedure, Some("(IN p_days int)")),
            obj(
                "order_total",
                Kind::Function,
                Some("(p_user int) → decimal(12,2)"),
            ),
        ],
    );
    let cols = |names: &[(&str, &str)]| {
        names
            .iter()
            .map(|(n, t)| SchemaColumn {
                name: n.to_string(),
                data_type: t.to_string(),
            })
            .collect::<Vec<_>>()
    };
    s.columns.insert(
        ("shop".into(), "orders".into()),
        cols(&[
            ("id", "int"),
            ("user_id", "int"),
            ("amount", "decimal(12,2)"),
        ]),
    );
    s.columns.insert(
        ("shop".into(), "users".into()),
        cols(&[
            ("id", "int"),
            ("email", "varchar(255)"),
            ("name", "varchar(64)"),
        ]),
    );
    s
}

/// Completions for `sql` with the cursor where `|` is.
fn at(sql: &str, schema: &TestSchema) -> Completions {
    let cursor = sql.find('|').expect("a | marks the cursor");
    let sql = sql.replace('|', "");
    complete(&sql, cursor, schema)
}

fn labels(sql: &str) -> Vec<String> {
    at(sql, &shop())
        .items
        .into_iter()
        .map(|c| c.label)
        .collect()
}

fn of_kind(sql: &str, kind: Kind) -> Vec<String> {
    at(sql, &shop())
        .items
        .into_iter()
        .filter(|c| c.kind == kind)
        .map(|c| c.label)
        .collect()
}

#[test]
fn keywords_follow_the_case_being_typed() {
    let first = |sql: &str| at(sql, &shop()).items[0].insert.clone();
    assert_eq!(first("sel|"), "select");
    assert_eq!(first("SEL|"), "SELECT");
    assert_eq!(first("Sel|"), "SELECT");
}

#[test]
fn after_from_come_tables() {
    let l = labels("SELECT * FROM ord|");
    assert_eq!(&l[..2], ["orders", "order items"]);
    assert!(of_kind("SELECT * FROM |", Kind::Column).is_empty());
    assert!(labels("SELECT * FROM sh|").contains(&"shop".to_string()));
    assert_eq!(labels("SELECT * FROM orders o JOIN us|")[0], "users");
    assert_eq!(labels("SELECT * FROM orders, us|")[0], "users");
    assert_eq!(labels("UPDATE us|")[0], "users");
    assert_eq!(labels("INSERT INTO ord|")[0], "orders");
}

#[test]
fn aliases_resolve_to_their_tables() {
    // in the table's own order
    assert_eq!(
        labels("SELECT o.| FROM orders o"),
        ["id", "user_id", "amount"]
    );
    assert_eq!(
        of_kind(
            "SELECT u.| FROM orders o JOIN users AS u ON u.id = o.user_id",
            Kind::Column
        )
        .len(),
        3
    );
    assert_eq!(
        labels("SELECT u.em| FROM orders o JOIN users u ON u.id = o.user_id")[0],
        "email"
    );
    // the table name works too, and so does a db-qualified table
    assert_eq!(labels("SELECT orders.am| FROM orders")[0], "amount");
    assert_eq!(labels("SELECT shop.users.na| FROM shop.users")[0], "name");
}

#[test]
fn bare_columns_come_from_every_table_in_from() {
    let cols = of_kind(
        "SELECT * FROM orders o JOIN users u ON o.user_id = u.id WHERE |",
        Kind::Column,
    );
    for c in ["id", "user_id", "amount", "email", "name"] {
        assert!(cols.contains(&c.to_string()), "{c} in {cols:?}");
    }
    assert_eq!(labels("SELECT * FROM users WHERE em|")[0], "email");
    // columns rank above keywords and functions
    assert_eq!(labels("SELECT na| FROM users")[0], "name");
}

#[test]
fn subqueries_in_from_have_their_select_list() {
    assert_eq!(
        labels("SELECT t.| FROM (SELECT id, amount AS total FROM orders) t"),
        ["id", "total"]
    );
    // inside the subquery, its own FROM
    assert_eq!(
        labels("SELECT * FROM (SELECT o.| FROM orders o) t")[..3],
        ["id", "user_id", "amount"]
    );
    // correlated: the outer query's aliases are visible inside
    assert_eq!(
        of_kind(
            "SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.|)",
            Kind::Column
        ),
        ["id", "email", "name"]
    );
}

#[test]
fn routines_show_their_signatures() {
    let c = at("CALL ref|", &shop());
    assert_eq!(c.items[0].label, "refresh_stats");
    assert_eq!(c.items[0].detail.as_deref(), Some("(IN p_days int)"));
    let f = at("SELECT order_t| FROM users", &shop());
    assert_eq!(f.items[0].label, "order_total");
    assert_eq!(
        f.items[0].detail.as_deref(),
        Some("(p_user int) → decimal(12,2)")
    );
}

#[test]
fn databases_after_a_qualifier_or_use() {
    let c = at("SELECT * FROM shop.|", &shop());
    assert!(c.items.iter().any(|i| i.label == "orders"));
    // another database's objects aren't loaded yet: ask for them
    let c = at("SELECT * FROM analytics.|", &shop());
    assert_eq!(c.missing, [Need::Objects("analytics".into())]);
    assert_eq!(labels("USE ana|"), ["analytics"]);
}

#[test]
fn reports_what_isnt_loaded() {
    let mut s = shop();
    s.columns.remove(&("shop".into(), "users".into()));
    let c = at("SELECT u.| FROM users u", &s);
    assert!(c.items.is_empty());
    assert_eq!(c.missing, [Need::Columns("shop".into(), "users".into())]);
    let empty = TestSchema {
        current: Some("shop".into()),
        ..Default::default()
    };
    let c = at("SELECT * FROM |", &empty);
    assert!(c.missing.contains(&Need::Objects("shop".into())));
    assert!(c.missing.contains(&Need::Databases));
}

#[test]
fn quotes_names_that_need_it_and_replaces_the_prefix() {
    let c = at("SELECT * FROM orderit|", &shop());
    let item = c.items.iter().find(|i| i.label == "order items").unwrap();
    assert_eq!(item.insert, "`order items`");
    assert_eq!(c.replace, 14..21);
}

#[test]
fn nothing_inside_strings_or_comments() {
    assert!(labels("SELECT 'ord|' FROM orders").is_empty());
    assert!(labels("SELECT 1 -- ord|").is_empty());
    assert!(labels("SELECT /* ord| */ 1").is_empty());
    assert!(!labels("SELECT /* x */ ord| FROM orders").is_empty());
}

#[test]
fn fuzzy_matching() {
    assert_eq!(labels("SELECT * FROM orders WHERE usid|")[0], "user_id");
    assert!(score("user_id", "uid").is_some());
    assert!(score("user_id", "xyz").is_none());
    assert!(score("orders", "ord") > score("old_records", "ord"));
}

#[test]
fn fast_on_a_big_schema() {
    let mut s = shop();
    let big: Vec<SchemaObject> = (0..5000)
        .map(|i| obj(&format!("t_{i:04}"), Kind::Table, None))
        .collect();
    s.objects.insert("shop".into(), big);
    let sql = "SELECT * FROM t_42";
    let started = Instant::now();
    let runs = 10;
    for _ in 0..runs {
        let c = complete(sql, sql.len(), &s);
        assert_eq!(c.items[0].label, "t_4200");
    }
    let each = started.elapsed() / runs;
    assert!(each.as_millis() < 50, "{each:?} per completion");
}

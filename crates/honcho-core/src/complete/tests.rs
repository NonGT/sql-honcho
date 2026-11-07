use std::collections::HashMap;
use std::time::Instant;

use super::*;

#[derive(Default)]
struct TestSchema {
    dialect: Dialect,
    current: Option<String>,
    databases: Option<Vec<String>>,
    objects: HashMap<String, Vec<SchemaObject>>,
    columns: HashMap<(String, Option<String>, String), Vec<SchemaColumn>>,
}

impl Schema for TestSchema {
    fn dialect(&self) -> Dialect {
        self.dialect
    }
    fn current_database(&self) -> Option<&str> {
        self.current.as_deref()
    }
    fn databases(&self) -> Option<&[String]> {
        self.databases.as_deref()
    }
    fn objects(&self, database: &str) -> Option<&[SchemaObject]> {
        self.objects.get(database).map(Vec::as_slice)
    }
    fn columns(
        &self,
        database: &str,
        schema: Option<&str>,
        table: &str,
    ) -> Option<&[SchemaColumn]> {
        self.columns
            .get(&(
                database.to_string(),
                schema.map(str::to_string),
                table.to_string(),
            ))
            .map(Vec::as_slice)
    }
}

fn obj(name: &str, kind: Kind, detail: Option<&str>) -> SchemaObject {
    SchemaObject {
        schema: None,
        name: name.into(),
        kind,
        detail: detail.map(Into::into),
    }
}

fn cols(names: &[(&str, &str)]) -> Vec<SchemaColumn> {
    names
        .iter()
        .map(|(n, t)| SchemaColumn {
            name: n.to_string(),
            data_type: t.to_string(),
        })
        .collect()
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
    s.columns.insert(
        ("shop".into(), None, "orders".into()),
        cols(&[
            ("id", "int"),
            ("user_id", "int"),
            ("amount", "decimal(12,2)"),
        ]),
    );
    s.columns.insert(
        ("shop".into(), None, "users".into()),
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
    s.columns.remove(&("shop".into(), None, "users".into()));
    let c = at("SELECT u.| FROM users u", &s);
    assert!(c.items.is_empty());
    assert_eq!(
        c.missing,
        [Need::Columns("shop".into(), None, "users".into())]
    );
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

/// SQL Server: `app` with tables in dbo and in sales.
fn mssql() -> TestSchema {
    let in_schema = |schema: &str, name: &str, kind: Kind, detail: Option<&str>| SchemaObject {
        schema: Some(schema.into()),
        ..obj(name, kind, detail)
    };
    let mut s = TestSchema {
        dialect: Dialect::SqlServer,
        current: Some("app".into()),
        databases: Some(vec!["app".into(), "analytics".into()]),
        ..Default::default()
    };
    s.objects.insert(
        "app".into(),
        vec![
            in_schema("dbo", "users", Kind::Table, None),
            in_schema("sales", "orders", Kind::Table, None),
            in_schema("sales", "close day", Kind::Procedure, Some("(@day date)")),
            in_schema(
                "dbo",
                "order_total",
                Kind::Function,
                Some("(@u int) → money"),
            ),
        ],
    );
    s.objects.insert(
        "analytics".into(),
        vec![in_schema("report", "daily", Kind::View, None)],
    );
    s.columns.insert(
        ("app".into(), Some("sales".into()), "orders".into()),
        cols(&[("id", "int"), ("amount", "money")]),
    );
    s.columns.insert(
        ("app".into(), None, "users".into()),
        cols(&[("id", "int"), ("email", "nvarchar(255)")]),
    );
    s
}

fn items(sql: &str, s: &TestSchema) -> Vec<(String, String)> {
    at(sql, s)
        .items
        .into_iter()
        .filter(|c| c.kind != Kind::Keyword && c.kind != Kind::Function || c.detail.is_some())
        .map(|c| (c.label, c.insert))
        .collect()
}

fn pair(label: &str, insert: &str) -> (String, String) {
    (label.into(), insert.into())
}

#[test]
fn sql_server_names_go_through_schemas() {
    let s = mssql();
    // tables outside dbo are written with their schema
    let from = items("SELECT * FROM |", &s);
    assert!(from.contains(&pair("users", "users")), "{from:?}");
    assert!(from.contains(&pair("orders", "sales.orders")), "{from:?}");
    assert!(from.contains(&pair("sales", "sales")), "{from:?}");
    // schema.: what's in it
    assert_eq!(
        items("SELECT * FROM sales.|", &s),
        [pair("orders", "orders")]
    );
    // db.: its schemas; db.schema.: the objects there
    assert_eq!(
        items("SELECT * FROM analytics.|", &s),
        [pair("report", "report")]
    );
    assert_eq!(
        items("SELECT * FROM analytics.report.|", &s),
        [pair("daily", "daily")]
    );
    // columns through schema-qualified tables and their aliases
    assert_eq!(
        items("SELECT o.| FROM sales.orders o", &s),
        [pair("id", "id"), pair("amount", "amount")]
    );
    assert_eq!(
        items("SELECT sales.orders.am| FROM sales.orders", &s)[0],
        pair("amount", "amount")
    );
    assert_eq!(
        items("SELECT u.| FROM [app].[dbo].users u", &s).len(),
        0,
        "dbo is asked for by name"
    );
    let c = at("SELECT u.| FROM [app].[dbo].users u", &s);
    assert_eq!(
        c.missing,
        [Need::Columns(
            "app".into(),
            Some("dbo".into()),
            "users".into()
        )]
    );
}

#[test]
fn sql_server_routines_and_quoting() {
    let s = mssql();
    let exec = at("EXEC clo|", &s);
    assert_eq!(exec.items[0].insert, "sales.[close day]");
    assert_eq!(exec.items[0].detail.as_deref(), Some("sales · (@day date)"));
    // T-SQL needs the schema on a function call
    let f = at("SELECT order_t| FROM users", &s);
    assert_eq!(f.items[0].insert, "dbo.order_total");
    // # is a temp table, not a comment; backslashes don't escape
    assert!(!labels_in("SELECT * FROM #t WHERE em|", &s).is_empty());
    assert!(!labels_in("SELECT 'C:\\' AS p, em| FROM users", &s).is_empty());
}

fn labels_in(sql: &str, s: &TestSchema) -> Vec<String> {
    at(sql, s).items.into_iter().map(|c| c.label).collect()
}

#[test]
fn postgres_quotes_and_dollar_bodies() {
    let in_schema = |schema: &str, name: &str, kind, detail| SchemaObject {
        schema: Some(schema.into()),
        ..obj(name, kind, detail)
    };
    let mut s = TestSchema {
        dialect: Dialect::Postgres,
        current: Some("app".into()),
        databases: Some(vec!["app".into()]),
        ..Default::default()
    };
    s.objects.insert(
        "app".into(),
        vec![
            in_schema("public", "users", Kind::Table, None),
            in_schema("public", "Orders", Kind::Table, None),
            in_schema("sales", "daily", Kind::Table, None),
        ],
    );
    let from = items("SELECT * FROM |", &s);
    assert!(from.contains(&pair("users", "users")), "{from:?}");
    // unquoted, Orders would mean orders
    assert!(from.contains(&pair("Orders", "\"Orders\"")), "{from:?}");
    assert!(from.contains(&pair("daily", "sales.daily")), "{from:?}");
    // nothing completes inside a dollar-quoted body
    assert!(labels_in("SELECT $$ FROM us|", &s).is_empty());
    assert!(!labels_in("SELECT $$x$$ FROM us|", &s).is_empty());
    // $1 is a parameter, not a quote
    assert!(!labels_in("SELECT * FROM us| WHERE id = $1", &s).is_empty());
}

#[test]
fn keywords_and_functions_follow_the_dialect() {
    let with = |dialect| TestSchema {
        dialect,
        current: Some("app".into()),
        databases: Some(vec!["app".into()]),
        objects: HashMap::from([("app".into(), vec![obj("users", Kind::Table, None)])]),
        ..Default::default()
    };
    let offered =
        |dialect, sql: &str, word: &str| labels_in(sql, &with(dialect)).iter().any(|l| l == word);
    assert!(offered(
        Dialect::SqlServer,
        "SELECT 1 NONCL|",
        "NONCLUSTERED"
    ));
    assert!(!offered(
        Dialect::Postgres,
        "SELECT 1 NONCL|",
        "NONCLUSTERED"
    ));
    assert!(!offered(Dialect::MySql, "SELECT 1 NONCL|", "NONCLUSTERED"));
    assert!(offered(
        Dialect::Postgres,
        "SELECT * FROM users WHERE email ILI|",
        "ILIKE"
    ));
    assert!(!offered(
        Dialect::MySql,
        "SELECT * FROM users WHERE email ILI|",
        "ILIKE"
    ));
    assert!(offered(Dialect::MySql, "SELECT * FROM users LIM|", "LIMIT"));
    assert!(!offered(
        Dialect::SqlServer,
        "SELECT * FROM users LIM|",
        "LIMIT"
    ));

    assert!(offered(Dialect::SqlServer, "SELECT GETD|", "GETDATE"));
    assert!(!offered(Dialect::Postgres, "SELECT GETD|", "GETDATE"));
    assert!(offered(Dialect::MySql, "SELECT IFN|", "IFNULL"));
    assert!(!offered(Dialect::SqlServer, "SELECT IFN|", "IFNULL"));
    assert!(offered(Dialect::Postgres, "SELECT DATE_TR|", "DATE_TRUNC"));
    assert!(offered(Dialect::Postgres, "SELECT COAL|", "COALESCE"));
}

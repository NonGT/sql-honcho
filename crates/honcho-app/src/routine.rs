//! Turning server objects into scripts that recreate them, and routines
//! into calls.

use honcho_db::{DriverKind, ObjectKind, ObjectRef, ParamMode, RoutineParam};

/// MySQL's dialect; the test driver speaks it too.
fn mysql_like(kind: DriverKind) -> bool {
    matches!(kind, DriverKind::MySql | DriverKind::Fake)
}

pub fn ident(kind: DriverKind, name: &str) -> String {
    match kind {
        DriverKind::SqlServer => format!("[{}]", name.replace(']', "]]")),
        DriverKind::Postgres | DriverKind::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
        _ => format!("`{}`", name.replace('`', "``")),
    }
}

pub fn qualified(kind: DriverKind, o: &ObjectRef) -> String {
    [
        o.database.as_deref(),
        o.schema.as_deref(),
        Some(o.name.as_str()),
    ]
    .into_iter()
    .flatten()
    .map(|part| ident(kind, part))
    .collect::<Vec<_>>()
    .join(".")
}

/// Objects that open with an Apply button: replacing them is routine work.
/// Tables don't, since recreating one would lose its rows.
pub fn editable(o: &ObjectRef) -> bool {
    matches!(
        o.kind,
        ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Trigger | ObjectKind::View
    )
}

/// A definition as a script that replaces the object when run. MySQL has
/// no CREATE OR REPLACE for routines and triggers, so those are dropped
/// first, and their bodies go between DELIMITER lines; views use CREATE OR
/// REPLACE.
pub fn script(kind: DriverKind, o: &ObjectRef, definition: &str) -> String {
    let body = definition.trim_end().trim_end_matches(';').trim_end();
    if !mysql_like(kind) {
        return format!("{body};\n");
    }
    let drop = match o.kind {
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::View => {
            let replaced = match body.get(..7) {
                Some(head) if head.eq_ignore_ascii_case("CREATE ") => {
                    format!("CREATE OR REPLACE {}", &body[7..])
                }
                _ => body.to_string(),
            };
            return format!("{replaced};\n");
        }
        _ => return format!("{body};\n"),
    };
    format!(
        "DROP {drop} IF EXISTS {};\nDELIMITER //\n{body} //\nDELIMITER ;\n",
        qualified(kind, o)
    )
}

/// Recreates an object from its definition without dropping anything
/// first: for putting back an object an Apply removed and then failed to
/// create.
pub fn restore_script(kind: DriverKind, o: &ObjectRef, definition: &str) -> String {
    let body = definition.trim_end().trim_end_matches(';').trim_end();
    if mysql_like(kind) && o.kind != ObjectKind::View {
        format!("DELIMITER //\n{body} //\nDELIMITER ;\n")
    } else {
        format!("{body};\n")
    }
}

/// Name the trial create uses; dropped again straight after.
pub const TRIAL_NAME: &str = "__honcho_apply_check";

/// For an Apply that has to drop first (MySQL routines and triggers): the
/// script's CREATE renamed to a throwaway name, and the DROP that removes
/// it. Creating that first shows the CREATE works, privileges and DEFINER
/// included, before the real object is dropped. `None` when there's no
/// CREATE to try or the object can be replaced in one step.
pub fn trial(kind: DriverKind, o: &ObjectRef, script: &str) -> Option<(String, String)> {
    if !mysql_like(kind) {
        return None;
    }
    let keyword = match o.kind {
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Trigger => "TRIGGER",
        _ => return None,
    };
    let statements = honcho_db::script::split(kind, script);
    let create = statements.iter().find(|s| {
        s.text
            .split_whitespace()
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("CREATE"))
    })?;
    let text = create.text;
    // the name follows the keyword: `name`, name or `db`.`name`
    let upper = text.to_ascii_uppercase();
    let at = upper.match_indices(keyword).map(|(i, _)| i).find(|&i| {
        let before = upper[..i].chars().next_back();
        let after = upper[i + keyword.len()..].chars().next();
        before.is_some_and(char::is_whitespace) && after.is_some_and(char::is_whitespace)
    })?;
    let name_start = at + keyword.len() + text[at + keyword.len()..].len()
        - text[at + keyword.len()..].trim_start().len();
    let rest = &text[name_start..];
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b'`' => {
                end += 1;
                while end < bytes.len() && bytes[end] != b'`' {
                    end += 1;
                }
                end += 1;
            }
            b'.' => end += 1,
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 => end += 1,
            _ => break,
        }
    }
    if end == 0 {
        return None;
    }
    let temp = ObjectRef {
        name: TRIAL_NAME.into(),
        ..o.clone()
    };
    let renamed = format!(
        "{}{}{}",
        &text[..name_start],
        qualified(kind, &temp),
        &rest[end.min(rest.len())..]
    );
    Some((
        format!("DELIMITER //\n{renamed} //\nDELIMITER ;\n"),
        format!("DROP {keyword} IF EXISTS {};\n", qualified(kind, &temp)),
    ))
}

/// The definition as the server reports it, minus differences that don't
/// mean someone changed it.
pub fn same_definition(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_end_matches(';')
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    };
    norm(a) == norm(b)
}

fn numeric(data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    [
        "int", "decimal", "numeric", "float", "double", "real", "bit", "bool", "serial",
    ]
    .iter()
    .any(|n| t.contains(n))
}

/// What the user typed in the form, as a SQL value: blank is NULL, numbers
/// for numeric types stay bare, everything else is a quoted string.
pub fn literal(kind: DriverKind, data_type: &str, typed: &str) -> String {
    let v = typed.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("null") {
        return "NULL".into();
    }
    if numeric(data_type) && v.parse::<f64>().is_ok() {
        return v.to_string();
    }
    let mut escaped = typed.replace('\'', "''");
    if kind == DriverKind::MySql {
        escaped = escaped.replace('\\', "\\\\");
    }
    format!("'{escaped}'")
}

/// A script that calls the routine with these values: OUT and INOUT
/// parameters go through session variables, read back at the end.
pub fn call(kind: DriverKind, o: &ObjectRef, params: &[RoutineParam], values: &[String]) -> String {
    let name = qualified(kind, o);
    let value = |i: usize| {
        let p = &params[i];
        literal(kind, &p.data_type, values.get(i).map_or("", String::as_str))
    };
    if o.kind == ObjectKind::Function {
        let args: Vec<String> = (0..params.len()).map(value).collect();
        return format!(
            "SELECT {name}({}) AS {};\n",
            args.join(", "),
            ident(kind, &o.name)
        );
    }
    let var = |p: &RoutineParam| format!("@{}", p.name);
    let mut out = String::new();
    for (i, p) in params.iter().enumerate() {
        if p.mode == ParamMode::InOut {
            out.push_str(&format!("SET {} = {};\n", var(p), value(i)));
        }
    }
    let args: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(i, p)| match p.mode {
            ParamMode::In => value(i),
            ParamMode::Out | ParamMode::InOut => var(p),
        })
        .collect();
    out.push_str(&format!("CALL {name}({});\n", args.join(", ")));
    let outs: Vec<String> = params
        .iter()
        .filter(|p| p.mode != ParamMode::In)
        .map(|p| format!("{} AS {}", var(p), ident(kind, &p.name)))
        .collect();
    if !outs.is_empty() {
        out.push_str(&format!("SELECT {};\n", outs.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: ObjectKind, name: &str) -> ObjectRef {
        ObjectRef::new(kind, name).in_database("app")
    }

    fn param(name: &str, mode: ParamMode, ty: &str) -> RoutineParam {
        RoutineParam {
            name: name.into(),
            mode,
            data_type: ty.into(),
        }
    }

    #[test]
    fn routines_are_dropped_and_recreated() {
        let p = obj(ObjectKind::Procedure, "refresh_stats");
        assert_eq!(
            script(
                DriverKind::MySql,
                &p,
                "CREATE DEFINER=`root`@`%` PROCEDURE `refresh_stats`()\nBEGIN\n  SELECT 1;\nEND"
            ),
            "DROP PROCEDURE IF EXISTS `app`.`refresh_stats`;\nDELIMITER //\n\
             CREATE DEFINER=`root`@`%` PROCEDURE `refresh_stats`()\nBEGIN\n  SELECT 1;\nEND //\n\
             DELIMITER ;\n"
        );
        let v = obj(ObjectKind::View, "active_users");
        assert_eq!(
            script(
                DriverKind::MySql,
                &v,
                "CREATE ALGORITHM=UNDEFINED VIEW `active_users` AS select 1"
            ),
            "CREATE OR REPLACE ALGORITHM=UNDEFINED VIEW `active_users` AS select 1;\n"
        );
        // a table is only shown, never dropped
        let t = obj(ObjectKind::Table, "users");
        assert_eq!(
            script(DriverKind::MySql, &t, "CREATE TABLE `users` (x int)"),
            "CREATE TABLE `users` (x int);\n"
        );
        assert!(!editable(&t));
        assert!(editable(&p));
    }

    #[test]
    fn trial_creates_under_a_throwaway_name() {
        let p = obj(ObjectKind::Procedure, "refresh_stats");
        let def = "CREATE DEFINER=`root`@`%` PROCEDURE `refresh_stats`(IN p_days INT)\nBEGIN\n  SELECT 1;\nEND";
        let script = script(DriverKind::MySql, &p, def);
        let (create, drop) = trial(DriverKind::MySql, &p, &script).unwrap();
        assert_eq!(
            create,
            "DELIMITER //\nCREATE DEFINER=`root`@`%` PROCEDURE `app`.`__honcho_apply_check`(IN p_days INT)\nBEGIN\n  SELECT 1;\nEND //\nDELIMITER ;\n"
        );
        assert_eq!(
            drop,
            "DROP PROCEDURE IF EXISTS `app`.`__honcho_apply_check`;\n"
        );
        // qualified and unquoted names, and the word inside the body is left alone
        let f = obj(ObjectKind::Function, "f");
        let (create, _) = trial(
            DriverKind::MySql,
            &f,
            "DELIMITER //\nCREATE FUNCTION app.f() RETURNS text RETURN 'FUNCTION x' //\nDELIMITER ;",
        )
        .unwrap();
        assert!(
            create.contains(
                "FUNCTION `app`.`__honcho_apply_check`() RETURNS text RETURN 'FUNCTION x'"
            ),
            "{create}"
        );
        // views replace in one step
        assert!(trial(
            DriverKind::MySql,
            &obj(ObjectKind::View, "v"),
            "CREATE OR REPLACE VIEW v AS SELECT 1"
        )
        .is_none());
        // nothing to try without a CREATE
        assert!(trial(DriverKind::MySql, &p, "DROP PROCEDURE p").is_none());
    }

    #[test]
    fn definitions_compare_without_noise() {
        assert!(same_definition(
            "CREATE PROCEDURE p()\nBEGIN  \nEND;\n",
            "CREATE PROCEDURE p()\nBEGIN\nEND"
        ));
        assert!(!same_definition(
            "CREATE PROCEDURE p() SELECT 1",
            "CREATE PROCEDURE p() SELECT 2"
        ));
    }

    #[test]
    fn literals_follow_the_type() {
        let mysql = DriverKind::MySql;
        assert_eq!(literal(mysql, "int", "42"), "42");
        assert_eq!(literal(mysql, "decimal(12,2)", "1.50"), "1.50");
        assert_eq!(literal(mysql, "int", "abc"), "'abc'");
        assert_eq!(literal(mysql, "varchar(10)", "42"), "'42'");
        assert_eq!(
            literal(mysql, "varchar(10)", "O'Brien \\"),
            "'O''Brien \\\\'"
        );
        assert_eq!(literal(mysql, "date", ""), "NULL");
        assert_eq!(literal(mysql, "text", "null"), "NULL");
    }

    #[test]
    fn calls_read_back_out_parameters() {
        let p = obj(ObjectKind::Procedure, "transfer");
        let params = [
            param("p_from", ParamMode::In, "int"),
            param("p_amount", ParamMode::InOut, "decimal(12,2)"),
            param("p_note", ParamMode::Out, "varchar(100)"),
        ];
        assert_eq!(
            call(
                DriverKind::MySql,
                &p,
                &params,
                &["7".into(), "10.5".into(), String::new()]
            ),
            "SET @p_amount = 10.5;\n\
             CALL `app`.`transfer`(7, @p_amount, @p_note);\n\
             SELECT @p_amount AS `p_amount`, @p_note AS `p_note`;\n"
        );
        let f = obj(ObjectKind::Function, "order_total");
        assert_eq!(
            call(
                DriverKind::MySql,
                &f,
                &[param("p_user", ParamMode::In, "int")],
                &["1".into()]
            ),
            "SELECT `app`.`order_total`(1) AS `order_total`;\n"
        );
        let none = obj(ObjectKind::Procedure, "refresh_stats");
        assert_eq!(
            call(DriverKind::MySql, &none, &[], &[]),
            "CALL `app`.`refresh_stats`();\n"
        );
    }
}

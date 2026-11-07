use super::*;

const PROC: &str = "CREATE DEFINER=`root`@`%` PROCEDURE `refresh_stats`(IN p_days INT)\nBEGIN\n  SELECT p_days * 2;\nEND";

fn proc_ref() -> ObjectRef {
    ObjectRef::new(ObjectKind::Procedure, "refresh_stats").in_database("app")
}

#[test]
fn names_survive_the_file_system() {
    for name in ["refresh_stats", "a/b:c", "ราคา", "50%_off", "x.y"] {
        let path = Project::file_for(ObjectKind::Procedure, None, name).unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap();
        assert!(!stem.contains('/') && !stem.contains(':'), "{stem}");
        assert_eq!(object_name(stem), name);
    }
    assert_eq!(
        Project::file_for(ObjectKind::View, None, "active_users").unwrap(),
        Path::new("views/active_users.sql")
    );
}

#[test]
fn files_leave_out_definer_and_database() {
    let text = file_text(DriverKind::MySql, &proc_ref(), PROC);
    assert_eq!(
        text,
        "DROP PROCEDURE IF EXISTS `refresh_stats`;\nDELIMITER //\n\
         CREATE PROCEDURE `refresh_stats`(IN p_days INT)\nBEGIN\n  SELECT p_days * 2;\nEND //\n\
         DELIMITER ;\n"
    );
    // DEFINER inside the body is the body's business
    let def = "CREATE PROCEDURE p()\nBEGIN\n  SELECT 'DEFINER=x';\nEND";
    assert_eq!(without_definer(def), def);
    let view = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`localhost` SQL SECURITY DEFINER VIEW `v` AS select 1 AS `x`";
    assert_eq!(
        without_definer(view),
        "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select 1 AS `x`"
    );
}

#[test]
fn a_pulled_file_matches_the_server() {
    let p = proc_ref();
    let file = file_text(DriverKind::MySql, &p, PROC);
    assert_eq!(normalise(&file, p.kind), normalise(PROC, p.kind));
    // hand-written, with a database, unquoted and CRLF
    let typed = "DELIMITER $$\r\nCREATE PROCEDURE app.refresh_stats(IN p_days INT)\r\nBEGIN\r\n  SELECT p_days * 2;  \r\nEND$$\r\nDELIMITER ;\r\n";
    assert_eq!(normalise(typed, p.kind), normalise(PROC, p.kind));
    let edited = file.replace("* 2", "* 3");
    assert_ne!(normalise(&edited, p.kind), normalise(PROC, p.kind));

    let v = ObjectRef::new(ObjectKind::View, "v");
    let server = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 1 AS `x`";
    let file = file_text(DriverKind::MySql, &v, server);
    assert!(
        file.starts_with("CREATE OR REPLACE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v`"),
        "{file}"
    );
    assert_eq!(normalise(&file, v.kind), normalise(server, v.kind));
}

#[test]
fn status_says_which_side_moved() {
    let k = ObjectKind::Procedure;
    let file = file_text(DriverKind::MySql, &proc_ref(), PROC);
    let edited = file.replace("* 2", "* 3");
    let moved = PROC.replace("* 2", "* 4");
    let seen = Seen {
        file: hash(&normalise(&file, k)),
        server: hash(&normalise(PROC, k)),
    };
    assert_eq!(status_of(k, Some(&file), Some(PROC), None), Status::Same);
    assert_eq!(
        status_of(k, Some(&edited), Some(PROC), None),
        Status::Differs
    );
    assert_eq!(
        status_of(k, Some(&edited), Some(PROC), Some(seen)),
        Status::Edited
    );
    assert_eq!(
        status_of(k, Some(&file), Some(&moved), Some(seen)),
        Status::ServerChanged
    );
    assert_eq!(
        status_of(k, Some(&edited), Some(&moved), Some(seen)),
        Status::BothChanged
    );
    assert_eq!(status_of(k, Some(&file), None, None), Status::FileOnly);
    assert_eq!(status_of(k, None, Some(PROC), None), Status::ServerOnly);
    // applied, then reformatted by the server: still the same
    let applied = Seen {
        file: hash(&normalise(&edited, k)),
        server: hash(&normalise(&moved, k)),
    };
    assert_eq!(
        status_of(k, Some(&edited), Some(&moved), Some(applied)),
        Status::Same
    );
}

fn entry(kind: ObjectKind, name: &str, text: &str) -> Entry {
    Entry {
        object: ObjectRef::new(kind, name),
        path: Project::file_for(kind, None, name).unwrap(),
        file: Some(text.into()),
        server: None,
        status: Status::FileOnly,
    }
}

#[test]
fn applies_in_dependency_order() {
    let mut entries = vec![
        entry(
            ObjectKind::Trigger,
            "t",
            "CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET @a = 1",
        ),
        entry(
            ObjectKind::Procedure,
            "p",
            "CREATE PROCEDURE p() SELECT * FROM a_view",
        ),
        entry(
            ObjectKind::View,
            "a_view",
            "CREATE VIEW a_view AS SELECT * FROM b_view",
        ),
        entry(
            ObjectKind::View,
            "b_view",
            "CREATE VIEW b_view AS SELECT f(1) FROM c_view_base",
        ),
        entry(ObjectKind::View, "c_view", "CREATE VIEW c_view AS SELECT 1"),
        entry(
            ObjectKind::Function,
            "f",
            "CREATE FUNCTION f(x INT) RETURNS INT RETURN x",
        ),
    ];
    order(&mut entries);
    let names: Vec<&str> = entries.iter().map(|e| e.object.name.as_str()).collect();
    // c_view_base isn't c_view: whole words only
    assert_eq!(names, ["f", "b_view", "a_view", "c_view", "p", "t"]);
}

#[test]
fn pull_records_a_baseline_kept_out_of_git() {
    let dir = tempfile::tempdir().unwrap();
    let project = Project::init(
        dir.path(),
        Manifest {
            connection: "dev".into(),
            database: "app".into(),
        },
    )
    .unwrap();
    assert_eq!(
        Project::open(dir.path()).unwrap().manifest,
        project.manifest
    );
    let e = Entry {
        object: proc_ref(),
        path: Project::file_for(ObjectKind::Procedure, None, "refresh_stats").unwrap(),
        file: None,
        server: Some(PROC.into()),
        status: Status::ServerOnly,
    };
    pull(&project, DriverKind::MySql, &e).unwrap();
    let files = project.files().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].1, None);
    assert_eq!(files[0].2, "refresh_stats");
    let seen = project.baseline().seen[&key(ObjectKind::Procedure, None, "refresh_stats")];
    assert_eq!(seen.server, hash(&normalise(PROC, ObjectKind::Procedure)));
    assert_eq!(
        fs::read_to_string(dir.path().join(".sql-honcho/.gitignore")).unwrap(),
        "*\n"
    );
    // a baseline taken against another database is ignored
    let other = Project {
        manifest: Manifest {
            database: "app2".into(),
            ..project.manifest.clone()
        },
        ..project.clone()
    };
    assert!(other.baseline().seen.is_empty());
}

#[test]
fn sql_server_files_carry_the_schema() {
    let path = Project::file_for(ObjectKind::Procedure, Some("sales"), "x.y").unwrap();
    assert_eq!(path, Path::new("procedures/sales.x%2Ey.sql"));

    let dir = tempfile::tempdir().unwrap();
    let project = Project::init(
        dir.path(),
        Manifest {
            connection: "dev".into(),
            database: "app".into(),
        },
    )
    .unwrap();
    let def = "-- totals\nCREATE PROC [sales].[x.y] @id int AS\nSELECT @id;";
    let e = Entry {
        object: ObjectRef::new(ObjectKind::Procedure, "x.y")
            .in_database("app")
            .in_schema("sales"),
        path,
        file: None,
        server: Some(def.into()),
        status: Status::ServerOnly,
    };
    pull(&project, DriverKind::SqlServer, &e).unwrap();
    let files = project.files().unwrap();
    assert_eq!(
        (files[0].1.as_deref(), files[0].2.as_str()),
        (Some("sales"), "x.y")
    );
    let text = &files[0].3;
    assert!(
        text.contains("CREATE OR ALTER PROC [sales].[x.y]"),
        "{text}"
    );
    assert_eq!(
        status_of(ObjectKind::Procedure, Some(text), Some(def), None),
        Status::Same
    );
}

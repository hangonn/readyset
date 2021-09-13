use mysql::prelude::*;
use noria_client::test_helpers::{self, sleep, Deployment};
use noria_client::BackendBuilder;
use serial_test::serial;

mod common;
use common::MySQLAdapter;

fn setup(deployment: &Deployment) -> mysql::Opts {
    test_helpers::setup::<MySQLAdapter>(
        BackendBuilder::new().require_authentication(false),
        deployment,
        true,
        true,
    )
}

#[test]
#[serial]
fn create_table() {
    let d = Deployment::new("create_table");
    let opts = setup(&d);

    let mut conn = mysql::Conn::new(opts).unwrap();

    conn.query_drop("CREATE TABLE Cats (id int, PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.query_drop("INSERT INTO Cats (id) VALUES (1)").unwrap();
    sleep();

    let row: Option<(i32,)> = conn
        .query_first("SELECT Cats.id FROM Cats WHERE Cats.id = 1")
        .unwrap();
    assert_eq!(row, Some((1,)))
}

#[test]
#[serial]
#[ignore] // alter table not supported yet
fn add_column() {
    let d = Deployment::new("create_table");
    let opts = setup(&d);

    let mut conn = mysql::Conn::new(opts).unwrap();

    conn.query_drop("CREATE TABLE Cats (id int, PRIMARY KEY(id))")
        .unwrap();
    sleep();

    conn.query_drop("INSERT INTO Cats (id) VALUES (1)").unwrap();
    sleep();

    let row: Option<(i32,)> = conn
        .query_first("SELECT Cats.id FROM Cats WHERE Cats.id = 1")
        .unwrap();
    assert_eq!(row, Some((1,)));

    conn.query_drop("ALTER TABLE Cats ADD COLUMN name TEXT;")
        .unwrap();
    conn.query_drop("UPDATE Cats SET name = 'Whiskers' WHERE id = 1;")
        .unwrap();
    sleep();

    let row: Option<(i32, String)> = conn
        .query_first("SELECT Cats.id, Cats.name FROM Cats WHERE Cats.id = 1")
        .unwrap();
    assert_eq!(row, Some((1, "Whiskers".to_owned())));
}

#[test]
#[serial]
fn json_column_insert_read() {
    let d = Deployment::new("insert_quoted_string");
    let opts = setup(&d);
    let mut conn = mysql::Conn::new(opts).unwrap();

    conn.query_drop("CREATE TABLE Cats (id int PRIMARY KEY, data JSON)")
        .unwrap();
    sleep();

    conn.query_drop("INSERT INTO Cats (id, data) VALUES (1, '{\"name\": \"Mr. Mistoffelees\"}')")
        .unwrap();
    sleep();
    sleep();

    let rows: Vec<(i32, String)> = conn.query("SELECT * FROM Cats WHERE Cats.id = 1").unwrap();
    assert_eq!(
        rows,
        vec![(1, "{\"name\":\"Mr. Mistoffelees\"}".to_string())]
    );
}

#[test]
#[serial]
fn json_column_partial_update() {
    let d = Deployment::new("insert_quoted_string");
    let opts = setup(&d);
    let mut conn = mysql::Conn::new(opts).unwrap();

    conn.query_drop("CREATE TABLE Cats (id int PRIMARY KEY, data JSON)")
        .unwrap();
    sleep();

    conn.query_drop("INSERT INTO Cats (id, data) VALUES (1, '{\"name\": \"xyz\"}')")
        .unwrap();
    conn.query_drop("UPDATE Cats SET data = JSON_REMOVE(data, '$.name') WHERE id = 1")
        .unwrap();
    sleep();

    let rows: Vec<(i32, String)> = conn.query("SELECT * FROM Cats WHERE Cats.id = 1").unwrap();
    assert_eq!(rows, vec![(1, "{}".to_string())]);
}

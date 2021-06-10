use anyhow::{anyhow, bail, Context};
use colored::*;
use itertools::Itertools;
use mysql_async as mysql;
use mysql_async::prelude::Queryable;
use mysql_async::Row;
use slog::o;
use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::convert::TryInto;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use zookeeper::{WatchedEvent, ZooKeeper, ZooKeeperExt};

use msql_srv::MysqlIntermediary;
use nom_sql::SelectStatement;
use noria::{ControllerHandle, ZookeeperAuthority};
use noria_client::backend::mysql_connector::MySqlConnector;
use noria_client::backend::noria_connector::NoriaConnector;
use noria_client::backend::BackendBuilder;
use noria_server::{Builder, ReuseConfigType};

use crate::ast::{Query, QueryResults, Record, SortMode, Statement, StatementResult, Value};
use crate::parser;

#[derive(Debug, Clone)]
pub struct TestScript {
    path: PathBuf,
    records: Vec<Record>,
}

impl From<Vec<Record>> for TestScript {
    fn from(records: Vec<Record>) -> Self {
        TestScript {
            path: "".into(),
            records,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub deployment_name: String,
    pub zookeeper_host: String,
    pub zookeeper_port: u16,
    pub use_mysql: bool,
    pub mysql_host: String,
    pub mysql_port: u16,
    pub mysql_user: String,
    pub mysql_db: String,
    pub disable_reuse: bool,
    pub verbose: bool,
    pub binlog_url: Option<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            deployment_name: "sqllogictest".to_string(),
            zookeeper_host: "127.0.0.1".to_string(),
            zookeeper_port: 2181,
            use_mysql: false,
            mysql_host: "localhost".to_string(),
            mysql_port: 3306,
            mysql_user: "root".to_string(),
            mysql_db: "sqllogictest".to_string(),
            disable_reuse: false,
            verbose: false,
            binlog_url: None,
        }
    }
}

impl RunOptions {
    pub fn zookeeper_addr(&self) -> String {
        format!("{}:{}", self.zookeeper_host, self.zookeeper_port)
    }

    fn zookeeper_authority(&self) -> ZookeeperAuthority {
        ZookeeperAuthority::new(&format!(
            "{}/{}",
            self.zookeeper_addr(),
            self.deployment_name
        ))
        .unwrap()
    }

    fn logger(&self) -> slog::Logger {
        if self.verbose {
            noria_server::logger_pls()
        } else {
            slog::Logger::root(slog::Discard, o!())
        }
    }

    pub fn mysql_opts_no_db(&self) -> mysql::Opts {
        mysql::OptsBuilder::default()
            .ip_or_hostname(self.mysql_host.clone())
            .tcp_port(self.mysql_port)
            .user(Some(self.mysql_user.clone()))
            .into()
    }

    pub fn mysql_opts(&self) -> mysql::Opts {
        mysql::OptsBuilder::from_opts(self.mysql_opts_no_db())
            .db_name(Some(self.mysql_db.clone()))
            .into()
    }
}

impl TestScript {
    pub fn read<R: io::Read>(path: PathBuf, input: R) -> anyhow::Result<Self> {
        let records = parser::read_records(input)?;
        Ok(Self { path, records })
    }

    pub fn open_file(path: PathBuf) -> anyhow::Result<Self> {
        let file = File::open(&path)?;
        Self::read(path, file)
    }

    pub fn name(&self) -> Cow<'_, str> {
        match self.path.file_name() {
            Some(n) => n.to_string_lossy(),
            None => Cow::Borrowed("unknown"),
        }
    }

    pub async fn run(&self, opts: RunOptions) -> anyhow::Result<()> {
        println!(
            "==> {} {}",
            "Running test script".bold(),
            self.path.canonicalize()?.to_string_lossy().blue()
        );

        if opts.use_mysql {
            self.recreate_test_database(opts.mysql_opts().clone(), &opts.mysql_db)
                .await?;
            let mut conn = mysql::Conn::new(opts.mysql_opts())
                .await
                .with_context(|| "connecting to mysql")?;

            self.run_on_mysql(&mut conn, false).await?;
        } else {
            if let Some(binlog_url) = &opts.binlog_url {
                self.recreate_test_database(binlog_url.try_into().unwrap(), &opts.mysql_db)
                    .await?;
            }

            self.run_on_noria(&opts).await?;

            // Cleanup zookeeper
            if let Ok(z) = ZooKeeper::connect(
                &opts.zookeeper_addr(),
                Duration::from_secs(3),
                |_: WatchedEvent| {},
            ) {
                z.delete_recursive(&format!("/{}", opts.deployment_name))
                    .unwrap_or(());
            }
        };

        println!(
            "{}",
            format!(
                "==> Successfully ran {} operations against {}",
                self.records.len(),
                if opts.use_mysql { "MySQL" } else { "Noria" }
            )
            .bold()
        );

        if opts.binlog_url.is_some() {
            // After all tests are done, we don't want to drop Noria right away, as it may cause
            // some conflicts between binlog propagation and cleanup
            std::thread::sleep(Duration::from_millis(250));
        }

        Ok(())
    }

    /// Establish a connection to MySQL and recreate the test database
    async fn recreate_test_database<S: AsRef<str>>(
        &self,
        opts: mysql::Opts,
        db_name: &S,
    ) -> anyhow::Result<()> {
        let mut create_db_conn = mysql::Conn::new(opts)
            .await
            .with_context(|| "connecting to mysql")?;

        create_db_conn
            .query_drop(format!("DROP DATABASE IF EXISTS {}", db_name.as_ref()))
            .await
            .with_context(|| "dropping database")?;

        create_db_conn
            .query_drop(format!("CREATE DATABASE {}", db_name.as_ref()))
            .await
            .with_context(|| "creating database")?;

        Ok(())
    }

    /// Run the test script on Noria server
    pub async fn run_on_noria(&self, opts: &RunOptions) -> anyhow::Result<()> {
        let mut noria_handle = self.start_noria_server(&opts).await;
        let (adapter_task, conn_opts) = self.setup_mysql_adapter(&opts).await;

        let mut conn = mysql::Conn::new(conn_opts)
            .await
            .with_context(|| "connecting to noria-mysql")?;

        self.run_on_mysql(&mut conn, opts.binlog_url.is_some())
            .await?;

        // After all tests are done, stop the adapter
        adapter_task.abort();
        let _ = adapter_task.await;

        // Stop Noria
        noria_handle.shutdown();
        noria_handle.wait_done().await;

        Ok(())
    }

    pub async fn run_on_mysql(
        &self,
        conn: &mut mysql::Conn,
        needs_sleep: bool,
    ) -> anyhow::Result<()> {
        let mut prev_was_statement = false;

        for record in &self.records {
            match record {
                Record::Statement(stmt) => {
                    prev_was_statement = true;
                    self.run_statement(stmt, conn)
                        .await
                        .with_context(|| format!("Running statement {}", stmt.command))?
                }

                Record::Query(query) => {
                    if prev_was_statement && needs_sleep {
                        prev_was_statement = false;
                        // When binlog replication is enabled, we need to give the statements some time to propagate
                        // before we can issue the next query
                        std::thread::sleep(Duration::from_millis(250));
                    }

                    self.run_query(query, conn)
                        .await
                        .with_context(|| format!("Running query {}", query.query))?
                }
                Record::HashThreshold(_) => {}
                Record::Halt => break,
            }
        }
        Ok(())
    }

    async fn run_statement(&self, stmt: &Statement, conn: &mut mysql::Conn) -> anyhow::Result<()> {
        let res = conn.query_drop(&stmt.command).await;
        match stmt.result {
            StatementResult::Ok => {
                if let Err(e) = res {
                    bail!("Statement failed: {}", e);
                }
            }
            StatementResult::Error => {
                if res.is_ok() {
                    bail!("Statement should have failed, but succeeded");
                }
            }
        }
        Ok(())
    }

    async fn run_query(&self, query: &Query, conn: &mut mysql::Conn) -> anyhow::Result<()> {
        let results = if query.params.is_empty() {
            conn.query(&query.query).await?
        } else {
            conn.exec(&query.query, &query.params).await?
        };

        let mut rows = results
            .into_iter()
            .map(|mut row: Row| -> anyhow::Result<Vec<Value>> {
                match &query.column_types {
                    Some(column_types) => column_types
                        .iter()
                        .enumerate()
                        .map(|(col_idx, col_type)| -> anyhow::Result<Value> {
                            let val = row.take(col_idx).ok_or_else(|| {
                                anyhow!(
                                    "Row had the wrong number of columns: expected {}, but got {}",
                                    column_types.len(),
                                    row.len()
                                )
                            })?;
                            Ok(Value::from_mysql_value_with_type(val, col_type)
                                .with_context(|| format!("Converting value to {:?}", col_type))?)
                        })
                        .collect(),
                    None => {
                        row.unwrap()
                            .into_iter()
                            .map(|val| {
                                Ok(Value::try_from(val)
                                    .with_context(|| format!("Converting value"))?)
                            })
                            .collect()
                    }
                }
            });

        let vals: Vec<Value> = match query.sort_mode.unwrap_or_default() {
            SortMode::NoSort => rows.fold_ok(vec![], |mut acc, row| {
                acc.extend(row);
                acc
            })?,
            SortMode::RowSort => {
                let mut rows: Vec<_> = rows.try_collect()?;
                rows.sort();
                rows.into_iter().flatten().collect()
            }
            SortMode::ValueSort => {
                let mut vals = rows.fold_ok(vec![], |mut acc, row| {
                    acc.extend(row);
                    acc
                })?;
                vals.sort();
                vals
            }
        };

        match &query.results {
            QueryResults::Hash { count, digest } => {
                if *count != vals.len() {
                    bail!(
                        "Wrong number of results returned: expected {}, but got {}",
                        count,
                        vals.len(),
                    );
                }
                let actual_digest = Value::hash_results(&vals);
                if actual_digest != *digest {
                    bail!(
                        "Incorrect values returned from query, expected values hashing to {:x}, but got {:x}",
                        digest,
                        actual_digest
                    );
                }
            }
            QueryResults::Results(expected_vals) => {
                if vals != *expected_vals {
                    bail!(
                        "Incorrect values returned from query (left: expected, right: actual): \n{}",
                        pretty_assertions::Comparison::new(expected_vals, &vals)
                    )
                }
            }
        }
        Ok(())
    }

    async fn start_noria_server(
        &self,
        run_opts: &RunOptions,
    ) -> noria_server::Handle<ZookeeperAuthority> {
        let mut authority = run_opts.zookeeper_authority();
        let logger = run_opts.logger();

        let mut builder = Builder::default();
        authority.log_with(logger.clone());
        builder.log_with(logger);

        if run_opts.disable_reuse {
            builder.set_reuse(ReuseConfigType::NoReuse)
        }

        if let Some(binlog_url) = &run_opts.binlog_url {
            // Add the data base name to the mysql url, and set as binlog source
            builder.set_mysql_url(format!("{}/{}", binlog_url, run_opts.mysql_db));
        }

        builder.start(Arc::new(authority)).await.unwrap()
    }

    async fn setup_mysql_adapter(
        &self,
        run_opts: &RunOptions,
    ) -> (tokio::task::JoinHandle<()>, mysql::Opts) {
        let binlog_url = if let Some(binlog_url) = &run_opts.binlog_url {
            // Append the database name to the binlog mysql url
            Some(format!("{}/{}", binlog_url, run_opts.mysql_db))
        } else {
            None
        };

        let auto_increments: Arc<RwLock<HashMap<String, AtomicUsize>>> = Arc::default();
        let query_cache: Arc<RwLock<HashMap<SelectStatement, String>>> = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let zk_auth = run_opts.zookeeper_authority();

        let ch = ControllerHandle::new(zk_auth).await.unwrap();

        let task = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();

            let reader = NoriaConnector::new(
                ch.clone(),
                auto_increments.clone(),
                query_cache.clone(),
                None,
            );

            let backend_builder = BackendBuilder::new();

            let backend_builder = if let Some(url) = binlog_url {
                let writer = MySqlConnector::new(url.into());
                backend_builder.writer(writer.await)
            } else {
                let writer = NoriaConnector::new(ch, auto_increments, query_cache, None);
                backend_builder.writer(writer.await)
            };

            let backend = backend_builder
                .reader(reader.await)
                .require_authentication(false)
                .build();

            MysqlIntermediary::run_on_tcp(backend, s).await.unwrap();
        });

        (
            task,
            mysql::OptsBuilder::default().tcp_port(addr.port()).into(),
        )
    }

    /// Get a reference to the test script's records.
    pub fn records(&self) -> &[Record] {
        &self.records
    }
}

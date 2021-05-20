use crate::connector::{BinlogAction, MySqlBinlogConnector};
use crate::snapshot::MySqlReplicator;
use noria::{consensus::Authority, ReplicationOffset, TableOperation};
use noria::{ControllerHandle, ReadySetError, ReadySetResult, Table, ZookeeperAuthority};
use slog::{error, info, o, Discard, Logger};
use std::collections::{hash_map, HashMap};
use std::convert::TryInto;

/// An adapter that converts binlog actions into Noria API calls
pub struct MySqlNoriaAdapter<A: Authority + 'static> {
    /// The Noria API handle
    noria: ControllerHandle<A>,
    /// The binlog reader
    connector: MySqlBinlogConnector,
    /// A map of cached table mutators
    mutator_map: HashMap<String, Table>,
    /// Logger
    log: Logger,
}

#[derive(Default)]
pub struct Builder {
    addr: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
    db_name: Option<String>,
    zookeeper_address: Option<String>,
    deployment: Option<String>,
    server_id: Option<u32>,
    log: Option<Logger>,
}

impl Builder {
    /// Create a new builder with the given MySQL primary server address and port
    pub fn new<T: Into<String>>(addr: T, port: u16) -> Self {
        Builder {
            addr: addr.into(),
            port,
            ..Default::default()
        }
    }

    /// A user name for MySQL
    /// The user must have the following permissions:
    /// `SELECT` - to be able to perform a snapshot (WIP)
    /// `RELOAD` - to be able to flush tables and acquire locks for a snapshot (WIP)
    /// `SHOW DATABASES` - to see databases for a snapshot (WIP)
    /// `REPLICATION SLAVE` - to be able to connect and read the binlog
    /// `REPLICATION CLIENT` - to use SHOW MASTER STATUS, SHOW SLAVE STATUS, and SHOW BINARY LOGS;
    pub fn with_user<T: Into<String>>(mut self, user_name: Option<T>) -> Self {
        self.user = user_name.map(Into::into);
        self
    }

    /// The password for the MySQL user
    pub fn with_password<T: Into<String>>(mut self, password: Option<T>) -> Self {
        self.password = password.map(Into::into);
        self
    }

    /// The name of the database to filter, if none is provided all entries will be filtered out
    pub fn with_database<T: Into<String>>(mut self, db_name: Option<T>) -> Self {
        self.db_name = db_name.map(Into::into);
        self
    }

    /// The address of the zookeeper instance for Noria
    pub fn with_zookeeper_addr<T: Into<String>>(mut self, zookeeper_address: Option<T>) -> Self {
        self.zookeeper_address = zookeeper_address.map(Into::into);
        self
    }

    /// The name of the Noria deployment
    pub fn with_deployment<T: Into<String>>(mut self, deployment: Option<T>) -> Self {
        self.deployment = deployment.map(Into::into);
        self
    }

    /// The binlog replica must be assigned a unique `server_id` in the replica topology
    pub fn with_server_id(mut self, server_id: Option<u32>) -> Self {
        self.server_id = server_id;
        self
    }

    pub fn with_logger(mut self, log: Logger) -> Self {
        self.log = Some(log);
        self
    }

    async fn start_inner<A: Authority>(
        mysql_options: mysql_async::Opts,
        mut noria: ControllerHandle<A>,
        server_id: Option<u32>,
        log: slog::Logger,
    ) -> ReadySetResult<()> {
        // Attempt to retreive the latest replication offset from noria, if none is present
        // begin the snapshot process
        let pos = match noria.replication_offset().await?.map(Into::into) {
            None => {
                info!(log, "Taking database snapshot");

                let replicator_options = mysql_options.clone();
                let pool = mysql_async::Pool::new(replicator_options);
                let replicator = MySqlReplicator {
                    pool,
                    tables: None,
                    log: log.clone(),
                };

                replicator.replicate_to_noria(&mut noria, true).await?
            }
            Some(pos) => pos,
        };

        info!(log, "Binlog position {:?}", pos);

        let schemas = mysql_options
            .db_name()
            .map(|s| vec![s.to_string()])
            .unwrap_or_default();

        // TODO: it is possible that the binlog position from noria is no longer
        // present on the primary, in which case the connection will fail, and we would
        // need to perform a new snapshot
        let connector =
            MySqlBinlogConnector::connect(mysql_options, schemas, Some(pos), server_id).await?;

        info!(log, "MySQL connected");

        let mut adapter = MySqlNoriaAdapter {
            noria,
            connector,
            mutator_map: HashMap::new(),
            log,
        };

        adapter.main_loop().await
    }

    ///
    /// Finish the build and begin monitoring the binlog for changes
    /// If noria has no replication offset information, it will replicate the target database in its
    /// entirety to Noria before listening on the binlog
    /// The replication happens in stages:
    /// * READ LOCK is acquired on the database
    /// * Next binlog position is read
    /// * The recipe (schema) DDL is replicated and installed in Noria (replacing current recipe)
    /// * Each table is individually replicated into Noria
    /// * READ LOCK is released
    /// * Adapter keeps reading binlog from the next position keeping Noria up to date
    ///
    pub async fn start(self) -> ReadySetResult<()> {
        let zookeeper_address = self
            .zookeeper_address
            .unwrap_or_else(|| "127.0.0.1:2181".into());

        let deployment = self
            .deployment
            .ok_or_else(|| ReadySetError::ReplicationFailed("Missing deployment".into()))?;

        let authority = ZookeeperAuthority::new(&format!("{}/{}", zookeeper_address, deployment))?;
        let noria = noria::ControllerHandle::new(authority).await?;

        let mysql_options = mysql_async::OptsBuilder::default()
            .ip_or_hostname(self.addr)
            .tcp_port(self.port)
            .user(self.user)
            .pass(self.password)
            .db_name(self.db_name)
            .into();

        let log = self.log.unwrap_or_else(|| Logger::root(Discard, o!()));

        Self::start_inner(mysql_options, noria, self.server_id, log).await
    }

    ///
    /// Same as [`start`](Builder::start), but accepts a MySQL url for options
    /// and externally supplied Noria `ControllerHandle` and `log`.
    /// The MySQL url must contain the database name, and user and password if applicable.
    /// i.e. `mysql://user:pass%20word@localhost/database_name`
    ///
    pub async fn start_with_url<A: Authority>(
        mysql_url: &str,
        noria: ControllerHandle<A>,
        server_id: Option<u32>,
        log: slog::Logger,
    ) -> ReadySetResult<()> {
        let mysql_options = mysql_async::Opts::from_url(mysql_url).map_err(|e| {
            ReadySetError::ReplicationFailed(format!("Invalid MySQL URL format {}", e))
        })?;

        Self::start_inner(mysql_options, noria, server_id, log).await
    }
}

impl<A: Authority> MySqlNoriaAdapter<A> {
    /// Handle a single BinlogAction by calling the proper Noria RPC
    async fn handle_action(
        &mut self,
        action: BinlogAction,
        pos: Option<ReplicationOffset>,
    ) -> Result<(), ReadySetError> {
        match action {
            BinlogAction::SchemaChange(schema) => {
                // Send the query to Noria as is
                self.noria.extend_recipe_with_offset(&schema, pos).await?;
                self.clear_mutator_cache();
                Ok(())
            }

            BinlogAction::WriteRows { table, mut rows }
            | BinlogAction::DeleteRows { table, mut rows }
            | BinlogAction::UpdateRows { table, mut rows } => {
                // Send the rows as are
                let table_mutator = self.mutator_for_table(table).await?;
                if let Some(offset) = pos {
                    // Update the replication offset
                    rows.push(TableOperation::SetReplicationOffset(offset));
                }
                table_mutator.perform_all(rows).await
            }
        }
    }

    /// Loop over the actions
    async fn main_loop(&mut self) -> ReadySetResult<()> {
        loop {
            let (action, position) = self.connector.next_action().await?;
            info!(self.log, "{:?}", action);
            let offset = position.map(|r| r.try_into()).transpose()?;
            if let Err(err) = self.handle_action(action, offset).await {
                error!(self.log, "{}", err);
            }
        }
    }

    /// When schema changes there is a risk the cached mutators will no longer be in sync
    /// and we need to drop them all
    fn clear_mutator_cache(&mut self) {
        self.mutator_map.clear()
    }

    /// Get a mutator for a noria table from the cache if available, or fetch a new one
    /// from the controller and cache it
    async fn mutator_for_table(&mut self, name: String) -> Result<&mut Table, ReadySetError> {
        match self.mutator_map.entry(name) {
            hash_map::Entry::Occupied(o) => Ok(o.into_mut()),
            hash_map::Entry::Vacant(v) => {
                let table = self.noria.table(v.key()).await?;
                Ok(v.insert(table))
            }
        }
    }
}

//! Postgres operations Grove needs: create, clone and drop per-cluster
//! databases on a server the user already runs.

mod tools;
mod url;

pub use tools::find_pg_tool;
pub use url::{database_url, fresh_db_name};

use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_postgres::{Client, Config, NoTls};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("can't reach Postgres at {url}: {message}")]
    Connect { url: String, message: String },
    #[error("invalid Postgres URL `{0}`")]
    InvalidUrl(String),
    #[error("{0}")]
    Query(String),
    #[error("database `{template}` is in use by {} connection(s), so it can't be used as a template", .connections.len())]
    TemplateInUse {
        template: String,
        connections: Vec<Connection>,
    },
    #[error("`{0}` not found; install Postgres client tools (e.g. `brew install libpq`)")]
    ToolMissing(&'static str),
    #[error("{tool} failed: {stderr}")]
    Tool { tool: &'static str, stderr: String },
    /// pg_restore finished but reported errors; the database exists and is
    /// usually fine (e.g. ownership or extension comment warnings).
    #[error("pg_restore reported problems: {0}")]
    RestoreWarnings(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A session connected to a database, as listed by `pg_stat_activity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub pid: i32,
    pub application_name: String,
    pub client_addr: Option<String>,
}

impl std::fmt::Display for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let app = if self.application_name.is_empty() {
            "unnamed client"
        } else {
            &self.application_name
        };
        write!(f, "{app} (pid {})", self.pid)
    }
}

/// A Postgres server, addressed by URL (`postgres://user@host:port[/db]`).
#[derive(Debug, Clone)]
pub struct Postgres {
    url: String,
}

impl Postgres {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// URL of `db` on this server, for `DATABASE_URL`.
    pub fn database_url(&self, db: &str) -> String {
        database_url(&self.url, db)
    }

    async fn connect(&self, db: &str) -> Result<Client, DbError> {
        let mut config: Config = self
            .url
            .parse()
            .map_err(|_| DbError::InvalidUrl(self.url.clone()))?;
        config.dbname(db);
        config.connect_timeout(CONNECT_TIMEOUT);
        config.application_name("grove");
        let (client, connection) = config.connect(NoTls).await.map_err(|e| DbError::Connect {
            url: self.url.clone(),
            message: error_message(&e),
        })?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!("postgres connection closed: {e}");
            }
        });
        Ok(client)
    }

    /// Maintenance connection used for CREATE/DROP DATABASE.
    async fn admin(&self) -> Result<Client, DbError> {
        self.connect("postgres").await
    }

    /// Returns the server version string.
    pub async fn ping(&self) -> Result<String, DbError> {
        let client = self.admin().await?;
        let row = client
            .query_one("SHOW server_version", &[])
            .await
            .map_err(query_err)?;
        Ok(row.get(0))
    }

    pub async fn database_exists(&self, name: &str) -> Result<bool, DbError> {
        let client = self.admin().await?;
        let row = client
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&name])
            .await
            .map_err(query_err)?;
        Ok(row.is_some())
    }

    pub async fn create_empty(&self, name: &str) -> Result<(), DbError> {
        let client = self.admin().await?;
        client
            .batch_execute(&format!("CREATE DATABASE {}", quote_ident(name)))
            .await
            .map_err(query_err)
    }

    /// `CREATE DATABASE name TEMPLATE template`. Postgres refuses while
    /// anything else is connected to the template; that case becomes
    /// [`DbError::TemplateInUse`] with the blocking sessions.
    pub async fn create_from_template(&self, name: &str, template: &str) -> Result<(), DbError> {
        let client = self.admin().await?;
        let sql = format!(
            "CREATE DATABASE {} TEMPLATE {}",
            quote_ident(name),
            quote_ident(template)
        );
        match client.batch_execute(&sql).await {
            Ok(()) => Ok(()),
            Err(e) if error_message(&e).contains("is being accessed by other users") => {
                Err(DbError::TemplateInUse {
                    template: template.to_string(),
                    connections: self.connections(template).await?,
                })
            }
            Err(e) => Err(query_err(e)),
        }
    }

    /// Sessions connected to `db`, excluding our own.
    pub async fn connections(&self, db: &str) -> Result<Vec<Connection>, DbError> {
        let client = self.admin().await?;
        let rows = client
            .query(
                "SELECT pid, coalesce(application_name, ''), client_addr::text \
                 FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
                &[&db],
            )
            .await
            .map_err(query_err)?;
        Ok(rows
            .iter()
            .map(|r| Connection {
                pid: r.get(0),
                application_name: r.get(1),
                client_addr: r.get(2),
            })
            .collect())
    }

    pub async fn terminate_connections(&self, db: &str) -> Result<u64, DbError> {
        let client = self.admin().await?;
        client
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = $1 AND pid <> pg_backend_pid()",
                &[&db],
            )
            .await
            .map_err(query_err)
    }

    /// Drops the database, disconnecting its sessions first.
    pub async fn drop_database(&self, name: &str) -> Result<(), DbError> {
        let client = self.admin().await?;
        let ident = quote_ident(name);
        // WITH (FORCE) needs Postgres 13+.
        let forced = format!("DROP DATABASE IF EXISTS {ident} WITH (FORCE)");
        if client.batch_execute(&forced).await.is_ok() {
            return Ok(());
        }
        self.terminate_connections(name).await?;
        client
            .batch_execute(&format!("DROP DATABASE IF EXISTS {ident}"))
            .await
            .map_err(query_err)
    }

    /// Creates `target` and fills it with `pg_dump -Fc source | pg_restore`.
    /// Works while `source` is in use.
    pub async fn dump_restore(&self, source: &str, target: &str) -> Result<(), DbError> {
        let pg_dump = find_pg_tool("pg_dump").ok_or(DbError::ToolMissing("pg_dump"))?;
        let pg_restore = find_pg_tool("pg_restore").ok_or(DbError::ToolMissing("pg_restore"))?;

        self.create_empty(target).await?;

        let mut dump = Command::new(pg_dump)
            .arg("--format=custom")
            .arg("--no-owner")
            .arg("--no-privileges")
            .arg(format!("--dbname={}", self.database_url(source)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let dump_stdout: Stdio = dump
            .stdout
            .take()
            .expect("piped stdout")
            .try_into()
            .map_err(std::io::Error::other)?;

        let restore = Command::new(pg_restore)
            .arg("--no-owner")
            .arg("--no-privileges")
            .arg(format!("--dbname={}", self.database_url(target)))
            .stdin(dump_stdout)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let mut dump_err = String::new();
        let mut dump_stderr = dump.stderr.take().expect("piped stderr");
        let (dump_status, _, restore_out) = tokio::join!(
            dump.wait(),
            dump_stderr.read_to_string(&mut dump_err),
            restore.wait_with_output(),
        );
        let dump_status = dump_status?;
        let restore_out = restore_out?;

        if !dump_status.success() {
            let _ = self.drop_database(target).await;
            return Err(DbError::Tool {
                tool: "pg_dump",
                stderr: dump_err.trim().to_string(),
            });
        }
        if !restore_out.status.success() {
            let stderr = String::from_utf8_lossy(&restore_out.stderr)
                .trim()
                .to_string();
            return Err(DbError::RestoreWarnings(stderr));
        }
        Ok(())
    }
}

/// Quotes an identifier for SQL (`"na""me"`).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn error_message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db.message().to_string(),
        None => {
            let mut msg = e.to_string();
            let mut source = std::error::Error::source(e);
            while let Some(s) = source {
                msg = format!("{msg}: {s}");
                source = s.source();
            }
            msg
        }
    }
}

fn query_err(e: tokio_postgres::Error) -> DbError {
    DbError::Query(error_message(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("api_dev"), "\"api_dev\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    /// Runs against a real server when `GROVE_TEST_PG_URL` is set.
    #[tokio::test]
    async fn lifecycle_against_real_server() {
        let Ok(url) = std::env::var("GROVE_TEST_PG_URL") else {
            eprintln!("skipping: GROVE_TEST_PG_URL not set");
            return;
        };
        let pg = Postgres::new(url);
        pg.ping().await.unwrap();

        let src = "grove_test_src";
        let tpl = "grove_test_tpl";
        let dump = "grove_test_dump";
        for db in [src, tpl, dump] {
            pg.drop_database(db).await.unwrap();
        }

        pg.create_empty(src).await.unwrap();
        assert!(pg.database_exists(src).await.unwrap());
        let client = pg.connect(src).await.unwrap();
        client
            .batch_execute("CREATE TABLE t (id int); INSERT INTO t VALUES (1), (2);")
            .await
            .unwrap();

        // In use by `client`, so TEMPLATE must fail with the blocking session.
        match pg.create_from_template(tpl, src).await {
            Err(DbError::TemplateInUse { connections, .. }) => assert!(!connections.is_empty()),
            other => panic!("expected TemplateInUse, got {other:?}"),
        }

        if find_pg_tool("pg_dump").is_some() {
            pg.dump_restore(src, dump).await.unwrap();
            let copy = pg.connect(dump).await.unwrap();
            let n: i64 = copy
                .query_one("SELECT count(*) FROM t", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(n, 2);
            drop(copy);
        }

        drop(client);
        for db in [src, tpl, dump] {
            pg.drop_database(db).await.unwrap();
            assert!(!pg.database_exists(db).await.unwrap());
        }
    }
}

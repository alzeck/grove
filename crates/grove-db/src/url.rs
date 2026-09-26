/// Replaces (or adds) the database in a `postgres://` URL, keeping the query.
pub fn database_url(server_url: &str, db: &str) -> String {
    let (base, query) = match server_url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (server_url, None),
    };
    let authority_start = base.find("://").map(|i| i + 3).unwrap_or(0);
    let base = match base[authority_start..].find('/') {
        Some(slash) => &base[..authority_start + slash],
        None => base,
    };
    match query {
        Some(q) => format!("{base}/{db}?{q}"),
        None => format!("{base}/{db}"),
    }
}

/// Name of a cluster's fresh copy of `shared`: `<shared>_<cluster>`, with
/// dashes turned into underscores, capped at Postgres' 63-byte limit.
pub fn fresh_db_name(shared: &str, cluster: &str) -> String {
    let name = format!("{shared}_{cluster}")
        .replace('-', "_")
        .to_ascii_lowercase();
    name.chars().take(63).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            database_url("postgres://me@localhost:5432", "api"),
            "postgres://me@localhost:5432/api"
        );
        assert_eq!(
            database_url("postgres://me:pw@localhost/other?sslmode=disable", "api"),
            "postgres://me:pw@localhost/api?sslmode=disable"
        );
        assert_eq!(
            database_url("postgresql://localhost/", "x"),
            "postgresql://localhost/x"
        );
    }

    #[test]
    fn fresh_names() {
        assert_eq!(fresh_db_name("api_dev", "pr-123"), "api_dev_pr_123");
        assert_eq!(fresh_db_name(&"a".repeat(70), "c").len(), 63);
    }
}

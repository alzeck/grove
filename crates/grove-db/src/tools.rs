use std::path::PathBuf;

/// Where Postgres client tools usually live on macOS when they're not on
/// the (GUI app's minimal) PATH.
const FALLBACK_DIRS: &[&str] = &[
    "/opt/homebrew/opt/libpq/bin",
    "/opt/homebrew/bin",
    "/usr/local/opt/libpq/bin",
    "/usr/local/bin",
    "/Applications/Postgres.app/Contents/Versions/latest/bin",
];

/// Finds `pg_dump`, `pg_restore`, … on PATH or in common install locations.
pub fn find_pg_tool(name: &str) -> Option<PathBuf> {
    if let Ok(p) = which::which(name) {
        return Some(p);
    }
    FALLBACK_DIRS
        .iter()
        .map(|d| PathBuf::from(d).join(name))
        .find(|p| p.is_file())
}

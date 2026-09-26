use serde::{Deserialize, Serialize};
use std::fmt;

/// A GitHub repository, `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// Parses a GitHub remote URL: `git@github.com:o/r.git`,
    /// `https://github.com/o/r`, `ssh://git@github.com/o/r.git`,
    /// `git://github.com/o/r`, … SSH URLs may use a host alias containing
    /// "github" (e.g. `git@github-work:o/r.git` from `~/.ssh/config`).
    /// Returns `None` for other hosts and for local paths.
    pub fn from_remote_url(url: &str) -> Option<RepoRef> {
        let url = url.trim();
        let (authority, path, ssh) = match url.split_once("://") {
            Some((scheme, rest)) => {
                let (authority, path) = rest.split_once('/')?;
                (authority, path, scheme.to_ascii_lowercase().contains("ssh"))
            }
            // scp-like syntax: [user@]host:path
            None => {
                let (authority, path) = url.split_once(':')?;
                if authority.contains('/') {
                    return None;
                }
                (authority, path, true)
            }
        };
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
        let is_github = matches!(
            host.as_str(),
            "github.com" | "www.github.com" | "ssh.github.com"
        ) || (ssh && host.contains("github"));
        if !is_github {
            return None;
        }
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        Self::from_slug(path)
    }

    /// Parses `owner/name`.
    pub(crate) fn from_slug(slug: &str) -> Option<RepoRef> {
        let (owner, name) = slug.split_once('/')?;
        let owner_ok =
            !owner.is_empty() && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let name_ok = !name.is_empty()
            && name != "."
            && name != ".."
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        (owner_ok && name_ok).then(|| RepoRef::new(owner, name))
    }

    /// GitHub names are case-insensitive.
    pub fn same_repo(&self, other: &RepoRef) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls() {
        let expected = Some(RepoRef::new("acme", "api"));
        for url in [
            "git@github.com:acme/api.git",
            "git@github.com:acme/api",
            "github.com:acme/api.git",
            "https://github.com/acme/api",
            "https://github.com/acme/api.git",
            "https://github.com/acme/api.git/",
            "https://token@github.com/acme/api.git",
            "http://www.github.com/acme/api",
            "ssh://git@github.com/acme/api.git",
            "ssh://git@ssh.github.com:443/acme/api.git",
            "git://github.com/acme/api",
            "git@github-work:acme/api.git",
            "  https://GitHub.com/acme/api\n",
        ] {
            assert_eq!(RepoRef::from_remote_url(url), expected, "{url}");
        }
        assert_eq!(
            RepoRef::from_remote_url("git@github.com:my-org/my.repo_2.git"),
            Some(RepoRef::new("my-org", "my.repo_2"))
        );
        for url in [
            "git@gitlab.com:acme/api.git",
            "https://gitlab.com/acme/api",
            "https://github-mirror.example.com/acme/api",
            "https://github.com/acme",
            "https://github.com/acme/api/tree/main",
            "/Users/me/api",
            "./a:b/c",
            "",
        ] {
            assert_eq!(RepoRef::from_remote_url(url), None, "{url}");
        }
    }

    #[test]
    fn display_and_comparison() {
        let a = RepoRef::new("Acme", "API");
        assert_eq!(a.to_string(), "Acme/API");
        assert!(a.same_repo(&RepoRef::new("acme", "api")));
        assert!(!a.same_repo(&RepoRef::new("acme", "web")));
        assert_ne!(a, RepoRef::new("acme", "api"));
    }
}

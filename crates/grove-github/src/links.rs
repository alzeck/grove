//! Recognising pull request references: URLs, `#123`, `owner/repo#123`.

use crate::repo::RepoRef;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// What the user typed to pick a PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrInput {
    /// A URL or `owner/repo#123`: the repo is known.
    Url(RepoRef, u64),
    /// `123` or `#123`: the caller decides which repo.
    Number(u64),
}

/// Parses `https://github.com/o/r/pull/123`, also with a trailing path
/// (`/files`), query string or fragment, and without the scheme.
pub fn parse_pr_url(s: &str) -> Option<(RepoRef, u64)> {
    let s = s.trim();
    let s = s.split(['?', '#']).next()?;
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let mut parts = rest.split('/');
    let host = parts.next()?.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next()? != "pull" {
        return None;
    }
    let number = parse_number(parts.next()?)?;
    Some((RepoRef::from_slug(&format!("{owner}/{name}"))?, number))
}

/// Accepts a PR URL, `123`, `#123` or `owner/repo#123`.
pub fn parse_pr_input(s: &str) -> Option<PrInput> {
    let s = s.trim();
    if let Some((repo, number)) = parse_pr_url(s) {
        return Some(PrInput::Url(repo, number));
    }
    if let Some(number) = parse_number(s.strip_prefix('#').unwrap_or(s)) {
        return Some(PrInput::Number(number));
    }
    let (slug, number) = s.rsplit_once('#')?;
    Some(PrInput::Url(
        RepoRef::from_slug(slug)?,
        parse_number(number)?,
    ))
}

/// A positive decimal number, digits only (no sign, no spaces).
fn parse_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|n| *n > 0)
}

static PR_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        // https://github.com/o/r/pull/n
        r"(?i:https?://(?:www\.)?github\.com)/([A-Za-z0-9-]+)/([A-Za-z0-9._-]+)/pull/(\d+)\b",
        // o/r#n, not preceded by something that makes it part of a path or word
        r"|(?:^|[^A-Za-z0-9_./-])([A-Za-z0-9-]+)/([A-Za-z0-9._-]+)#(\d+)\b",
    ))
    .expect("valid regex")
});

/// PR links in `body` (`https://github.com/o/r/pull/n` or `o/r#n`) to any of
/// the `candidates` repos, compared case-insensitively. Returns the matching
/// candidate (with its own casing) and PR number, deduplicated, in order of
/// first appearance.
pub fn find_linked_prs(body: &str, candidates: &[RepoRef]) -> Vec<(RepoRef, u64)> {
    let mut found: Vec<(RepoRef, u64)> = Vec::new();
    for caps in PR_LINK.captures_iter(body) {
        let groups = if caps.get(1).is_some() {
            [1, 2, 3]
        } else {
            [4, 5, 6]
        };
        let [owner, name, number] = groups.map(|i| caps.get(i).map_or("", |m| m.as_str()));
        let Some(number) = parse_number(number) else {
            continue;
        };
        let linked = RepoRef::new(owner, name);
        let Some(repo) = candidates.iter().find(|c| c.same_repo(&linked)) else {
            continue;
        };
        if !found.iter().any(|(r, n)| r == repo && *n == number) {
            found.push((repo.clone(), number));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, name: &str) -> RepoRef {
        RepoRef::new(owner, name)
    }

    #[test]
    fn pr_urls() {
        let expected = Some((repo("acme", "api"), 123));
        for url in [
            "https://github.com/acme/api/pull/123",
            "https://github.com/acme/api/pull/123/",
            "https://github.com/acme/api/pull/123/files",
            "https://github.com/acme/api/pull/123/files#diff-abc",
            "https://github.com/acme/api/pull/123#discussion_r1234",
            "https://github.com/acme/api/pull/123?notification_referrer_id=x",
            "http://www.github.com/acme/api/pull/123",
            "github.com/acme/api/pull/123",
            " https://github.com/acme/api/pull/123\n",
        ] {
            assert_eq!(parse_pr_url(url), expected, "{url}");
        }
        for url in [
            "https://github.com/acme/api/issues/123",
            "https://github.com/acme/api/pull/",
            "https://github.com/acme/api/pull/abc",
            "https://github.com/acme/api/pull/0",
            "https://gitlab.com/acme/api/pull/123",
            "https://github.com/acme/api",
        ] {
            assert_eq!(parse_pr_url(url), None, "{url}");
        }
    }

    #[test]
    fn pr_inputs() {
        assert_eq!(parse_pr_input("123"), Some(PrInput::Number(123)));
        assert_eq!(parse_pr_input(" #42 "), Some(PrInput::Number(42)));
        assert_eq!(
            parse_pr_input("acme/api#7"),
            Some(PrInput::Url(repo("acme", "api"), 7))
        );
        assert_eq!(
            parse_pr_input("https://github.com/acme/web/pull/9/files"),
            Some(PrInput::Url(repo("acme", "web"), 9))
        );
        for bad in [
            "",
            "#",
            "+5",
            "12a",
            "acme#1",
            "acme/api#",
            "acme/api#x",
            "feat/branch",
        ] {
            assert_eq!(parse_pr_input(bad), None, "{bad}");
        }
    }

    #[test]
    fn linked_prs() {
        let candidates = [repo("acme", "api"), repo("Acme", "Web")];
        let body = "\
Needs https://github.com/acme/web/pull/12 and acme/api#3.
Also (ACME/API#3) again, https://github.com/Acme/Api/pull/3/files,
[acme/web#12](https://github.com/acme/web/pull/12#issuecomment-1)
Unrelated: other/repo#5, https://github.com/other/repo/pull/6, #7,
an issue https://github.com/acme/api/issues/8, a path src/acme/api#9,
foo.acme/api#10, acme/api#11x, **acme/web#13**";
        assert_eq!(
            find_linked_prs(body, &candidates),
            vec![
                (repo("Acme", "Web"), 12),
                (repo("acme", "api"), 3),
                (repo("Acme", "Web"), 13),
            ]
        );
        assert!(find_linked_prs(body, &[]).is_empty());
        assert_eq!(
            find_linked_prs("acme/api#1 at start", &candidates),
            vec![(repo("acme", "api"), 1)]
        );
    }
}

//! GitHub support for Grove through the `gh` CLI: looking up pull requests,
//! and recognising PR references in user input and PR descriptions.

mod cli;
mod error;
mod links;
mod pr;
mod repo;

pub use cli::{auth_status, gh_version};
pub use error::GhError;
pub use links::{PrInput, find_linked_prs, parse_pr_input, parse_pr_url};
pub use pr::{PrState, PullRequest, fetch_refspec, pr_view};
pub use repo::RepoRef;

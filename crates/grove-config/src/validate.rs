//! Semantic checks that serde can't express: names, cross-references,
//! dependency cycles, template syntax, and domain collisions.

use crate::error::Diagnostic;
use crate::project::{ProjectConfig, ReadyKind};
use crate::template::Templates;
use crate::workspace::WorkspaceConfig;
use crate::{DEFAULT_CLUSTER, is_valid_process_name, is_valid_project_name};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) fn validate(
    ws: &WorkspaceConfig,
    projects: &IndexMap<String, (ProjectConfig, PathBuf)>,
    ws_file: &Path,
) -> Vec<Diagnostic> {
    let t = Templates::new();
    let mut out = Vec::new();
    let ws_diag = |msg: String| Diagnostic::new(Some(ws_file.to_path_buf()), msg);

    if ws.name.trim().is_empty() {
        out.push(ws_diag("workspace `name` must not be empty".into()));
    }
    if let Some([lo, hi]) = ws.port_range
        && (lo >= hi || lo < 1024)
    {
        out.push(ws_diag(format!(
            "port_range [{lo}, {hi}] must be ascending and above 1023"
        )));
    }
    for (label, tpl) in [
        ("domains.default", &ws.domains.default),
        ("domains.cluster", &ws.domains.cluster),
    ] {
        if let Err(e) = t.check_syntax(tpl) {
            out.push(ws_diag(format!("{label}: {e}")));
        }
    }

    // Hosts seen so far, for collision detection: host -> "project.process".
    let mut hosts: HashMap<(bool, String), String> = HashMap::new();

    for (name, (p, file)) in projects {
        let diag = |msg: String| Diagnostic::new(Some(file.clone()), msg);

        if !is_valid_project_name(name) {
            out.push(diag(format!(
                "project name `{name}` must match [a-z][a-z0-9-]*"
            )));
        }
        if p.repo.trim().is_empty() {
            out.push(diag("`repo` must not be empty".into()));
        }

        let mut check = |label: String, tpl: &str| {
            if let Err(e) = t.check_syntax(tpl) {
                out.push(Diagnostic::new(Some(file.clone()), format!("{label}: {e}")));
            }
        };
        for (i, cmd) in p.setup.iter().enumerate() {
            check(format!("setup[{i}]"), cmd);
        }
        for (i, cmd) in p.after_pull.iter().enumerate() {
            check(format!("after_pull[{i}]"), cmd);
        }
        for (i, cmd) in p.teardown.iter().enumerate() {
            check(format!("teardown[{i}]"), cmd);
        }
        for (k, v) in &p.env {
            check(format!("env.{k}"), v);
        }
        if let Some(db) = &p.database {
            if let Some(m) = &db.migrate {
                check("database.migrate".into(), m);
            }
            if let Some(s) = &db.seed {
                check("database.seed".into(), s);
            }
        }

        for (pname, proc_) in &p.processes {
            let diag = |msg: String| Diagnostic::new(Some(file.clone()), msg);
            if !is_valid_process_name(pname) {
                out.push(diag(format!(
                    "process name `{pname}` must match [a-z][a-z0-9_-]* and not be dir, db or branch"
                )));
            }
            if let Err(e) = t.check_syntax(&proc_.run) {
                out.push(diag(format!("processes.{pname}.run: {e}")));
            }
            for (k, v) in &proc_.env {
                if let Err(e) = t.check_syntax(v) {
                    out.push(diag(format!("processes.{pname}.env.{k}: {e}")));
                }
            }
            if let Some(ready) = &proc_.ready
                && matches!(ready.kind, ReadyKind::Http { .. } | ReadyKind::Tcp)
                && proc_.port.is_none()
            {
                out.push(diag(format!(
                    "processes.{pname}: an http/tcp ready check needs `port`"
                )));
            }
            if let Some(domain) = &proc_.domain {
                if proc_.port.is_none() {
                    out.push(diag(format!("processes.{pname}: `domain` needs `port`")));
                    continue;
                }
                for is_default in [true, false] {
                    let cluster = if is_default { DEFAULT_CLUSTER } else { "c" };
                    match ws
                        .domains
                        .render_host(&t, domain, name, pname, cluster, is_default)
                    {
                        Ok(host) => {
                            let owner = format!("{name}.{pname}");
                            if let Some(prev) = hosts.insert((is_default, host.clone()), owner) {
                                out.push(diag(format!(
                                    "processes.{pname}: domain `{host}` collides with {prev}; \
                                     include `{{{{ process }}}}` in the template"
                                )));
                            }
                        }
                        Err(e) => out.push(diag(format!("processes.{pname}.domain: {e}"))),
                    }
                }
            }
            for dep in &proc_.depends_on {
                let dep_project = dep.resolve_project(name);
                match projects.get(dep_project) {
                    None => out.push(diag(format!(
                        "processes.{pname}: depends on `{dep}` but project `{dep_project}` doesn't exist"
                    ))),
                    Some((dp, _)) if !dp.processes.contains_key(&dep.process) => {
                        out.push(diag(format!(
                            "processes.{pname}: depends on `{dep}` but that process doesn't exist"
                        )))
                    }
                    _ => {}
                }
            }
        }
    }

    out.extend(find_cycles(projects));
    out
}

/// Depth-first search over `project.process` nodes.
fn find_cycles(projects: &IndexMap<String, (ProjectConfig, PathBuf)>) -> Vec<Diagnostic> {
    let mut edges: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    for (name, (p, _)) in projects {
        for (pname, proc_) in &p.processes {
            let deps = proc_
                .depends_on
                .iter()
                .map(|d| (d.resolve_project(name).to_string(), d.process.clone()))
                .collect();
            edges.insert((name.clone(), pname.clone()), deps);
        }
    }

    let mut out = Vec::new();
    let mut done: HashSet<(String, String)> = HashSet::new();
    let mut reported: HashSet<(String, String)> = HashSet::new();
    let mut keys: Vec<_> = edges.keys().cloned().collect();
    keys.sort();

    for start in keys {
        let mut stack = vec![(start.clone(), 0usize)];
        let mut path: Vec<(String, String)> = vec![start.clone()];
        let mut on_path: HashSet<(String, String)> = HashSet::from([start.clone()]);
        if done.contains(&start) {
            continue;
        }
        while let Some((node, idx)) = stack.pop() {
            let next = edges.get(&node).and_then(|d| d.get(idx)).cloned();
            match next {
                Some(dep) => {
                    stack.push((node.clone(), idx + 1));
                    if on_path.contains(&dep) {
                        if reported.insert(dep.clone()) {
                            let pos = path.iter().position(|n| *n == dep).unwrap_or(0);
                            let cycle: Vec<String> = path[pos..]
                                .iter()
                                .chain(std::iter::once(&dep))
                                .map(|(p, n)| format!("{p}.{n}"))
                                .collect();
                            let file = projects.get(&dep.0).map(|(_, f)| f.clone());
                            out.push(Diagnostic::new(
                                file,
                                format!("dependency cycle: {}", cycle.join(" -> ")),
                            ));
                        }
                    } else if !done.contains(&dep) && edges.contains_key(&dep) {
                        on_path.insert(dep.clone());
                        path.push(dep.clone());
                        stack.push((dep, 0));
                    }
                }
                None => {
                    done.insert(node.clone());
                    on_path.remove(&node);
                    path.pop();
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(src: &str) -> (String, (ProjectConfig, PathBuf)) {
        let p: ProjectConfig = toml::from_str(src).unwrap();
        (
            p.name.clone(),
            (p.clone(), PathBuf::from(format!("{}.toml", p.name))),
        )
    }

    fn ws() -> WorkspaceConfig {
        toml::from_str("name = \"w\"").unwrap()
    }

    fn run(projects: &[&str]) -> Vec<String> {
        let map: IndexMap<_, _> = projects.iter().map(|s| project(s)).collect();
        validate(&ws(), &map, Path::new("grove.toml"))
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn valid_workspace_has_no_diagnostics() {
        let diags = run(&[r#"
            name = "api"
            repo = "r"
            [processes.web]
            run = "x"
            port = "PORT"
            domain = true
            [processes.worker]
            run = "y"
            depends_on = ["web"]
        "#]);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn detects_cycles_and_missing_refs() {
        let diags = run(&[
            r#"
            name = "a"
            repo = "r"
            [processes.one]
            run = "x"
            depends_on = ["two"]
            [processes.two]
            run = "x"
            depends_on = ["b.three"]
            [processes.four]
            run = "x"
            depends_on = ["missing", "nope.x"]
            "#,
            r#"
            name = "b"
            repo = "r"
            [processes.three]
            run = "x"
            depends_on = ["a.one"]
            "#,
        ]);
        assert!(diags.iter().any(|d| d.contains("cycle")), "{diags:?}");
        assert!(diags.iter().any(|d| d.contains("`missing`")), "{diags:?}");
        assert!(
            diags.iter().any(|d| d.contains("project `nope`")),
            "{diags:?}"
        );
        assert_eq!(
            diags.iter().filter(|d| d.contains("cycle")).count(),
            1,
            "{diags:?}"
        );
    }

    #[test]
    fn detects_domain_collisions_and_bad_templates() {
        let diags = run(&[r#"
            name = "api"
            repo = "r"
            [env]
            BAD = "{{ oops"
            [processes.web]
            run = "x"
            port = "PORT"
            domain = true
            [processes.admin]
            run = "x"
            port = "ADMIN_PORT"
            domain = true
            [processes.nope]
            run = "x"
            domain = true
        "#]);
        assert!(diags.iter().any(|d| d.contains("collides")), "{diags:?}");
        assert!(diags.iter().any(|d| d.contains("env.BAD")), "{diags:?}");
        assert!(
            diags.iter().any(|d| d.contains("`domain` needs `port`")),
            "{diags:?}"
        );
    }
}

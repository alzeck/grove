# Grove — Spec

Grove is a macOS desktop app (Rust + GPUI) for running several copies of a multi-repo dev environment side by side. Each copy is a **cluster**: a set of projects checked out on specific branches (usually as git worktrees), with their own ports, optionally their own Postgres database, and their own `https://*.localhost` domains.

Typical uses:

- Keep the **default cluster** (your main clones, your work in progress) running, and spin up a second cluster on a worktree to compare behaviour side by side.
- Create a cluster from a **pull request** without stopping or stashing your current work.

Grove is generic: nothing about a specific company's stack is hard-coded. Everything a project needs is described in a **workspace** config that can live in a team-owned repo.

## Goals

- One place to create, start, stop, pull, and tear down clusters.
- Projects fully described by config: setup, processes, database, domains, and wiring between projects.
- Built-in HTTPS reverse proxy with a local CA. No Caddy or other external proxy.
- Idle clusters stop themselves and wake up when you visit their URL.
- A CLI that drives the running app, usable by humans, scripts, and coding agents.

## Non-goals (v1)

- Linux or Windows. Platform-specific pieces (clonefile, Keychain, tray) are kept isolated so ports stay possible.
- Docker or containers. All processes run natively.
- Databases other than Postgres; extra services such as Redis or Elasticsearch.
- Git hosts other than GitHub.
- Clusters that keep running after the app quits (no daemon).
- Automatic pulling of new commits (pulling is a button).

---

## Concepts

| Term | Meaning |
|---|---|
| **Workspace** | A directory of config (`grove.toml` + `projects/*.toml`), local or cloned from a git URL. Usually team-owned. |
| **User config** | `~/.grove/config.toml`. Per-machine settings: which workspace, where clones live, which projects to enable, personal overrides. |
| **Project** | A repo plus its recipe: setup steps, processes, database, domains, env. |
| **Process** | A long-running command within a project (e.g. `web`, `worker`). Gets a port, optional domain, readiness check. |
| **Main clone** | The regular checkout of a project, e.g. `~/Developer/api`. |
| **Cluster** | A named set of project instances running together. Each project in a cluster is either a **worktree** (own checkout on a branch) or **reused** from the default cluster. |
| **Default cluster** | Always exists, named `default`. Runs every enabled project from its main clone, on whatever branch is checked out. Cannot be torn down. |
| **Instance** | One project inside one cluster: checkout + allocated ports + database choice + running processes. |

A cluster is not a Kubernetes cluster; the name just means "a group of things running together".

---

## Configuration

### Files and directories

Everything Grove owns lives under `~/.grove/`:

```
~/.grove/
  config.toml          # user config
  workspace/           # clone of the workspace repo (if configured by URL)
  worktrees/<cluster>/<project>/
  logs/<cluster>/<project>/<process>.log
  ca/                  # local root CA (key is 0600)
  state.json           # persisted clusters, port allocations, running pgids
  grove.sock           # IPC socket for the CLI
```

### User config — `~/.grove/config.toml`

```toml
workspace = "git@github.com:acme/grove-workspace.git"  # or a local path
clones_root = "~/Developer"
projects = ["api", "frontend"]         # which workspace projects this machine uses
editor = "zed {{ path }}"
idle_timeout = "30m"                   # overrides the workspace default

[postgres]
url = "postgres://juan@localhost:5432"

# Personal overrides, deep-merged over the project's config (arrays replace).
[overrides.api]
path = "~/work/api"                    # main clone lives somewhere other than clones_root
copy_from_original = [".env", ".env.local", "node_modules"]
```

- Main clone path defaults to `<clones_root>/<project>`. If it is missing, Grove offers to clone `repo` there.
- A workspace given by URL is cloned to `~/.grove/workspace/` and fast-forwarded on launch (skipped if dirty), plus a manual "Update workspace" action.
- Secrets never go in the workspace. They come from copied `.env` files or from user overrides.

### Workspace — `grove.toml`

```toml
name = "acme"
idle_timeout = "30m"

[domains]
default = "{{ project }}.localhost"                  # default cluster
cluster = "{{ project }}-{{ cluster }}.localhost"    # every other cluster

[postgres]
url = "postgres://localhost:5432"                    # user config may override
```

### Project — `projects/<name>.toml`

```toml
name = "api"                               # [a-z][a-z0-9-]*, used in domains and paths
repo = "git@github.com:acme/api.git"
default_branch = "main"                    # optional; detected from origin/HEAD

# Copied from the main clone into new worktrees. Paths or globs.
# Uses APFS clonefile (instant, copy-on-write); falls back to a normal copy.
copy_from_original = [".env", "node_modules", "apps/*/node_modules"]

setup = ["pnpm install --prefer-offline"]  # after worktree creation, and on "Re-run setup"
after_pull = ["pnpm install --prefer-offline"]
teardown = []                              # optional, before the worktree is removed

[env]                                      # applies to every process, setup and hook in this project
FRONTEND_URL = "{{ projects.frontend.web.url }}"

[database]
name = "api_dev"                           # the shared database
env = "DATABASE_URL"                       # env var that receives the connection URL
default = "shared"                         # "shared" | "fresh"; picked per cluster at creation
fresh = "dump"                             # how to build a fresh DB: "dump" | "template" | "migrate"
migrate = "pnpm db:migrate"                # run after any fresh DB is created
seed = "pnpm db:seed"                      # run only for fresh = "migrate"

[processes.web]
run = "pnpm dev"                           # run through the user's shell, in the checkout
cwd = "."                                  # relative to the checkout (monorepos)
port = { env = "PORT", default = 3000 }    # default cluster uses 3000; other clusters get a free port
ready = { http = "/health", timeout = "120s" }
domain = { wildcard = true }               # uses workspace templates; wildcard adds *.<host>
autostart = true

[processes.worker]
run = "pnpm worker"
depends_on = ["web"]                       # starts after web is ready
env = { QUEUE_PREFIX = "{{ cluster }}" }
```

Process fields:

- `ready`: one of `{ http = "/path" }` (2xx/3xx on the process port), `{ tcp = true }` (port accepts connections), `{ log = "regex" }` (output matches), or omitted (ready as soon as it has started). Default timeout is 120s.
- `domain`: `true`, or a table with `wildcard`, `default`, `cluster` (template overrides). Validation fails if two processes resolve to the same host.
- `stop_timeout`: default `10s`. Processes get SIGTERM on their process group, then SIGKILL.
- No automatic restart. Crashed processes show their exit code and a restart button.
- Reserved process names: `dir`, `db`, `branch`.

### Templating

Templates use [minijinja](https://github.com/mitsuhiko/minijinja) syntax (`{{ … }}`) everywhere: env values, domains, commands, editor.

| Variable | Value |
|---|---|
| `cluster` | Cluster name |
| `project`, `process` | Current project / process name |
| `self.dir` | Current checkout path |
| `projects.<p>.dir` | Checkout path of project `p` in this cluster |
| `projects.<p>.branch` | Branch checked out |
| `projects.<p>.db.url`, `.db.name` | Database for `p` in this cluster |
| `projects.<p>.<proc>.url` | `https://<host>` via the proxy |
| `projects.<p>.<proc>.host` | Host name only |
| `projects.<p>.<proc>.port` | Allocated port |
| `projects.<p>.<proc>.local_url` | `http://127.0.0.1:<port>`, best for server-to-server calls |
| `postgres.url` | Postgres server URL |

Use `projects["my-api"]` for names containing dashes. When a project is **reused**, its variables resolve to the default cluster's instance.

### Env injected into every process

Precedence, lowest to highest: captured shell env → Grove built-ins → project `[env]` → process `env` → user overrides.

- The port env var (e.g. `PORT`) and the database env var (e.g. `DATABASE_URL`).
- `GROVE_CLUSTER`, `GROVE_PROJECT`, `GROVE_PROCESS`.
- `NODE_EXTRA_CA_CERTS` pointing at Grove's CA certificate. It adds to Node's CA list rather than replacing it. `SSL_CERT_FILE` is deliberately not set, because it would replace the system bundle.

Apps that load `.env` with dotenv normally don't override variables that are already set, so Grove's values win.

**Shell environment.** GUI apps on macOS don't inherit your shell's `PATH`, so version managers (mise, asdf, nvm) and direnv would be missing. Grove captures the environment by running your login shell interactively in each checkout directory (as Zed and VS Code do), caches it per checkout, and refreshes it after `setup` and `after_pull`.

---

## Clusters

### Naming

Cluster names are DNS-safe: `[a-z0-9-]`, at most 30 characters, unique.

- From a PR: `pr-<number>`; on collision `pr-<project>-<number>`.
- From a branch: slug of the branch (`feat/new-thing` → `feat-new-thing`).
- Editable in the create dialog.

### Creating a cluster

The create dialog starts from a **PR** (URL or number) or a **branch** (existing, or new from the default branch). Then for each enabled project:

- **Checkout**: `worktree` on a branch, or `reuse` the default cluster's instance.
- **Database**: `shared` or `fresh` (defaults from config).

Warnings shown in the dialog:

- A reused project uses the default cluster's database, even if other projects in this cluster use a fresh one.
- A branch that is already checked out in another worktree or the main clone can't be checked out again (git limitation). Grove offers to use that checkout instead.

Steps for each worktree project, in order:

1. `git worktree add ~/.grove/worktrees/<cluster>/<project> <branch>` from the main clone.
2. Copy `copy_from_original` paths from the main clone.
3. If fresh: create the database (so `setup` can already use it).
4. Capture the shell env, then run `setup`.
5. If fresh: run `migrate`, then `seed` (for `fresh = "migrate"` only).
6. Allocate ports and register domains.
7. Start processes (if autostart) in `depends_on` order, waiting for readiness.

Progress and output of every step are shown live. A failed step leaves the cluster in an `error` state with a retry button; nothing is rolled back silently.

### From a pull request

- Requires the `gh` CLI, installed and authenticated (checked in diagnostics).
- `gh pr view --json …` provides the head branch, fork status, body, and state.
- Same-repo PRs: fetch the head branch and create a worktree tracking it, so you can push to it.
- Fork PRs: fetch `pull/<n>/head` into a local branch `pr-<n>`.
- **Linked PRs**: Grove scans the PR body for links (`https://github.com/o/r/pull/n` and `o/r#n`) to repos of other enabled projects. Those PRs are pre-selected as worktrees for their projects. Projects without a linked PR default to `reuse`, and the user can change any of them.
- The cluster view shows each PR's state (open, merged, closed), refreshed on demand.

### Pulling

A **Pull** button on each worktree project runs `git pull --ff-only`, then `after_pull`, then offers to restart that project's processes. If the branch has diverged, Grove shows git's error and does nothing else.

### Tearing down

The default cluster can't be torn down. For other clusters, Grove first shows a confirmation that lists:

- Worktrees with uncommitted or untracked changes (from `git status --porcelain`), with the file list.
- Fresh databases that will be dropped.
- A note that branches are kept.

Then: stop processes, run `teardown` hooks, `git worktree remove` (with `--force` only for dirty worktrees the user confirmed), drop fresh databases, free ports, remove domains, delete logs. The shared database is never dropped, and **branches are never deleted**.

### Adopting existing worktrees

Grove lists worktrees of each main clone (`git worktree list --porcelain`) that it didn't create, for example ones made by T3 Code, Claude Code, or `git worktree add`. They can be adopted into a new or existing cluster. Adopted worktrees are **not** removed on teardown unless the user ticks the option.

### Lifecycle and idle

Cluster states: `stopped → starting → running → idle → starting → …`, plus `error`. Each process has its own state (`stopped`, `starting`, `ready`, `crashed(code)`).

- **Idle**: if no HTTP request reaches any of a cluster's domains for `idle_timeout`, its processes are stopped and the cluster becomes `idle`. The default cluster is exempt by default. Open websockets, such as a hot-reload connection in a forgotten tab, don't count as activity. `0` disables idling.
- **Wake on request**: a request to an idle cluster starts it. Browser navigations (`Accept: text/html`) get a "Starting <cluster>…" page that reloads when the cluster is ready; other requests are held until ready (up to the ready timeout).
- **Quit**: closing the window keeps Grove in the menu bar. Quitting asks for confirmation, then stops everything. On the next launch every cluster is `stopped`.
- **Crash recovery**: running process groups are recorded in `state.json`. On launch, Grove kills leftover groups from a previous crash, checking process start times so a reused PID is never killed by mistake.

### Ports

- Default cluster: each process's `port.default` if set, otherwise allocated.
- Other clusters: allocated from a configurable range (default `4100–4999`), persisted per cluster and process so URLs and cookies stay stable across restarts.
- Ports are checked for availability before each start.

---

## Database (Postgres)

- Grove connects to an existing server (Homebrew, Postgres.app, …). It never installs or starts Postgres.
- **Shared**: `DATABASE_URL` points at `database.name`. Grove never migrates or drops it.
- **Fresh**: database named `<name>_<cluster>` (dashes become underscores), created per `fresh`:
  - `dump`: `pg_dump -Fc <shared> | pg_restore -d <new>`. Works while the default cluster is connected. **Recommended default.**
  - `template`: `CREATE DATABASE … TEMPLATE <shared>`. Fastest, but Postgres refuses while anything is connected to the shared database. Grove shows which connections are blocking and offers to stop the default cluster's processes for that project for the duration.
  - `migrate`: empty database, then `migrate` and `seed`.
- `migrate` runs after every fresh creation (the branch may add migrations).
- Operations use `tokio-postgres`. `dump` uses the `pg_dump` / `pg_restore` binaries.

---

## Proxy

Built into Grove. No Caddy.

- Listens on `127.0.0.1` and `::1`, ports 443 and 80 (80 redirects to HTTPS). macOS allows these without root. If a port is in use (e.g. Caddy is still running), diagnostics say which process holds it.
- **TLS**: at first run Grove generates a local root CA (`rcgen`) and installs it in the login keychain with `security add-trusted-cert` (one password prompt). Leaf certificates are minted on demand for each SNI host and cached, so wildcard subdomains need no special handling. Firefox needs `security.enterprise_roots.enabled` to trust the Keychain; the docs cover it.
- **Routing**: host → `127.0.0.1:<port>`, including `*.host` for wildcard processes. The `Host` header is preserved; `X-Forwarded-For/Proto/Host` are added. HTTP/1.1 and HTTP/2 to the browser, HTTP/1.1 upstream, websocket upgrades, streaming bodies (SSE).
- An unknown host gets a Grove page listing clusters and their URLs.
- Tracks the last request time per cluster for idling, and triggers wake on request.
- Crates: `hyper` 1.x, `hyper-util`, `rustls`, `tokio-rustls`, `rcgen`.

---

## UI (GPUI)

**Main window**

- Sidebar: clusters, with `default` pinned at the top, a state dot, and PR number/branch.
- Cluster view: one section per project showing the checkout (branch, dirty marker, ahead/behind, PR link and state, reused or worktree), processes (state, port, URL, start/stop/restart), and actions: Pull, Open in editor, Open in browser, Open shell, Re-run setup.
- Cluster actions: Start, Stop, Teardown.
- Terminal panes: every process runs in a PTY and is displayed with a libghostty-vt terminal. You can type into it (debuggers such as `binding.pry` and `pdb` work). "Open shell" opens an interactive shell in the checkout with the cluster's env. Tabs and a split view.
- Create-cluster dialog, as described above.
- Settings: workspace source, clones root, enabled projects, editor, Postgres URL, idle timeout, port range, "Trust CA", "Update workspace".
- Diagnostics: `git` and `gh` (installed, authenticated), Postgres reachable, ports 80/443 free, CA trusted, main clones present.

**Onboarding (first run)**: choose a workspace (path or URL) → clones root → pick projects → clone missing repos → trust the CA → start the default cluster.

**Menu bar**: clusters with state, start/stop, open URLs, show window, quit.

---

## CLI

A single `grove` binary. `grove` with no subcommand (or the .app bundle) launches the GUI. Subcommands talk to the running app over `~/.grove/grove.sock` (JSON lines; streaming for `-f`). If the app isn't running, the CLI launches it in the background and waits for the socket. Exceptions: `validate` and `doctor` work without the app.

```
grove ls [--json]
grove new --pr <url|number> [--project <p>] [--name <n>] [--fresh-db <p>…] [--reuse <p>…]
grove new --branch <branch> [--from <base>] [--name <n>] …
grove up <cluster>             # start (wakes an idle cluster)
grove down <cluster>           # stop
grove rm <cluster> [--yes]     # teardown; without --yes prints the safety report and asks
grove pull <cluster> [<project>]
grove logs <cluster> <project>/<process> [-f]
grove open <cluster> [<project>[/<process>]]
grove status [<cluster>] [--json]
grove validate [<workspace-path>]
grove doctor
```

`grove rm` never removes dirty worktrees without an explicit `--yes`, and the prompt lists what would be lost.

---

## Architecture

A single process with no daemon. The GPUI UI runs on the main thread. The core runs on a tokio multi-threaded runtime in background threads. The UI sends commands to the core over a channel and receives events on a broadcast channel, which update a GPUI entity holding the current snapshot. The IPC server is another client of the same command/event API, so the CLI and UI can't drift apart.

```
crates/
  grove-config   # serde schema, load + merge (workspace ⊕ user overrides), validation, minijinja templating
  grove-core     # orchestrator: clusters, state machines, ports, persistence, command/event API. Headless.
  grove-git      # shells out to `git` (respects user config, credential helpers, hooks); worktrees, status, clonefile copy
  grove-github   # `gh` wrapper, linked-PR parsing
  grove-db       # Postgres operations
  grove-proc     # PTY spawn, process groups, readiness, shell env capture, log files, orphan reaping
  grove-proxy    # reverse proxy, local CA, cert minting, idle tracking, wake
  grove-term     # libghostty-vt wrapper + GPUI terminal element
  grove-ipc      # protocol types, socket server + client
  grove-app      # bin `grove`: GPUI UI, menu bar, CLI entry point
```

- Shelling out to `git` rather than using `gix` keeps full fidelity with the user's git setup and has complete worktree support.
- GPUI comes from [GPUI Kit](https://gpui-kit.com) (`gpui-kit` on crates.io), which pins a published snapshot of Zed's GPUI (`gpui-pre`) and adds a styled component library. The plain `gpui` crate on crates.io is too old.
- `grove-core` is testable without UI; integration tests use temporary repos and a throwaway Postgres database.
- A hidden `grove headless` mode runs the core in the foreground (Ctrl-C stops everything) for development before the UI exists.

---

## Milestones

Each milestone ends with something runnable.

| # | Scope | Done when |
|---|---|---|
| M0 | Cargo workspace, CI (fmt, clippy, test), MIT licence, `examples/workspace` | `cargo test` passes in CI |
| M1 | `grove-config`: schema, merging, templating, validation | `grove validate examples/workspace` reports useful errors |
| M2 | `grove-proc` + core: PTYs, ports, env, `depends_on`, readiness, logs, orphan reaping | `grove headless` runs the example default cluster |
| M3 | Git, worktrees, clusters from branch and PR, linked PRs, databases, pull, teardown safety | `grove headless` creates, runs, and tears down a PR cluster |
| M4 | Proxy: CA, trust, TLS, routing, websockets, idle + wake | Both clusters reachable over HTTPS; an idle cluster wakes on visit |
| M5 | GPUI app: sidebar, cluster view, create dialog, terminal panes (libghostty-vt), menu bar | Usable daily without the CLI |
| M6 | Interactive shells, onboarding, settings, diagnostics | New machine set up from zero through the UI |
| M7 | CLI over IPC | Every CLI command above works against the running app |
| M8 | Adopt external worktrees, .app bundle packaging | Worktrees made by other tools appear and can be adopted |

---

## Risks and open questions

- **libghostty-vt** Rust bindings exist but their API is unstable, and the build probably needs Zig. Isolated in `grove-term`; verify the build story at the start of M5.
- **GPUI** has no stable API; `gpui-kit` is pinned to an exact version and updated deliberately.
- **Shell env capture** is the usual source of "works in terminal, not in app" bugs. It needs good diagnostics (show the captured env per checkout).
- **Postgres `TEMPLATE`** requires no connections to the source database, hence `dump` as the default.
- **Orphans** after a crash are only cleaned on the next launch. macOS has no parent-death signal.
- **Multiple databases per project** (e.g. Rails multi-db) are not supported in v1; the schema could later grow `[databases.<name>]`.
- **Firefox** trust store needs a manual pref.

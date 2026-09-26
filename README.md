# Grove

Run several copies of your multi-repo dev environment side by side.

Grove is a macOS app (Rust + [GPUI](https://gpui-kit.com)) that manages **clusters**: sets of projects checked out on specific branches, usually as git worktrees, each with its own ports, optionally its own Postgres database, and its own `https://*.localhost` domains.

- Keep your main checkout running while a second cluster runs a colleague's PR.
- Create a cluster from a pull request (linked PRs in other repos come along) without stashing your work.
- Every project is described in a shareable workspace config: setup steps, processes, database, domains, and how projects talk to each other.
- A built-in HTTPS proxy with a local certificate authority. No Caddy, no `/etc/hosts`.
- Idle clusters stop themselves and wake up when you open their URL.
- A CLI for everything, so scripts and coding agents can drive it too.

> Status: early. macOS only, Postgres only, GitHub only. See [SPEC.md](SPEC.md) for the design.

## Build

Requirements: Rust (the toolchain in `rust-toolchain.toml` is installed automatically by rustup), Zig 0.16.0 (builds the terminal emulator; with [mise](https://mise.jdx.dev), `mise trust && mise install` picks up the pin in `mise.toml`), git, and optionally the GitHub CLI (`gh`) and Postgres client tools (`brew install gh libpq`).

```sh
cargo build --release
./target/release/grove            # the app
./target/release/grove --help     # the CLI
```

## Try it with the demo

The demo builds two local repos (a tiny API and a web app), a workspace describing them, and a separate Grove home, so nothing touches `~/.grove`:

```sh
examples/demo/setup.sh /tmp/grove-demo
export GROVE_HOME=/tmp/grove-demo/home GROVE_HTTPS_PORT=8443 GROVE_HTTP_PORT=8480
grove                                     # or: grove headless --https-port 8443 --http-port 8480
grove up default
grove new --branch feature/hello --yes    # web gets a worktree, api is reused
grove ls
open https://web-feature-hello.localhost:8443
```

Trust Grove's certificate authority from Settings (or open the URLs with `curl --cacert $GROVE_HOME/ca/ca.pem`).

## Configure your own projects

A workspace is a directory (or git repo) with a `grove.toml` and one file per project in `projects/`. See [`examples/workspace`](examples/workspace) for a realistic example and [SPEC.md](SPEC.md#configuration) for every option.

```toml
# projects/api.toml
name = "api"
repo = "git@github.com:acme/api.git"
copy_from_original = [".env", "node_modules"]   # instant APFS copies into new worktrees
setup = ["pnpm install --prefer-offline"]

[database]
name = "api_dev"          # shared database; clusters can get a fresh copy instead
fresh = "dump"
migrate = "pnpm db:migrate"

[processes.server]
run = "pnpm dev"
port = { env = "PORT", default = 3000 }
ready = { http = "/health" }
domain = true             # https://api.localhost, https://api-<cluster>.localhost
```

Your personal settings live in `~/.grove/config.toml` (the app's onboarding writes it):

```toml
workspace = "git@github.com:acme/grove-workspace.git"   # or a local path
clones_root = "~/Developer"
editor = "zed {{ path }}"
```

Check a workspace with `grove validate path/to/workspace`.

## CLI

```
grove ls                                   list clusters
grove status [cluster]                     projects, processes, URLs
grove new --pr <url|#n> [--project p]      cluster from a pull request
grove new --branch <name> [--project p]    cluster from a branch
grove up|down|restart <cluster> [project/process]
grove logs <cluster> <project/process> -f
grove pull <cluster> <project>             git pull --ff-only + after_pull hooks
grove open <cluster> [project]             open in the browser
grove edit <cluster> <project>             open in your editor
grove rm <cluster>                         tear down (refuses to lose uncommitted work)
grove doctor                               check git, gh, Postgres, proxy, certificate
```

The CLI talks to the running app and starts it in the background if needed. `grove headless` runs everything without a window.

## Development

```sh
cargo test --workspace         # includes end-to-end tests with real repos and processes
cargo clippy --workspace --all-targets -- -D warnings
```

Crates: `grove-config` (schema, validation, templating), `grove-core` (orchestrator), `grove-proc` (PTY processes), `grove-git`, `grove-github`, `grove-db`, `grove-proxy` (HTTPS proxy + local CA), `grove-term` (libghostty terminal view), `grove-ipc`, `grove-app` (the `grove` binary).

## License

MIT

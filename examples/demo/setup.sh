#!/bin/sh
# Builds a self-contained Grove demo: two local git repos (api, web), a
# workspace describing them, and a Grove home that uses it.
#
#   examples/demo/setup.sh [dir]        (default: /tmp/grove-demo)
#   GROVE_HOME=<dir>/home grove headless --https-port 8443
set -eu

DEMO="${1:-/tmp/grove-demo}"
SRC="$(cd "$(dirname "$0")" && pwd)"
export GIT_AUTHOR_NAME=grove GIT_AUTHOR_EMAIL=grove@example.com
export GIT_COMMITTER_NAME=grove GIT_COMMITTER_EMAIL=grove@example.com

rm -rf "$DEMO"
mkdir -p "$DEMO/origin" "$DEMO/clones" "$DEMO/workspace/projects" "$DEMO/home"

for app in api web; do
  git init -q --bare -b main "$DEMO/origin/$app.git"
  git clone -q "$DEMO/origin/$app.git" "$DEMO/clones/$app" 2>/dev/null
  cp "$SRC/$app/server.py" "$DEMO/clones/$app/"
  printf 'DEMO_SECRET=from-main-clone\n' > "$DEMO/clones/$app/.env"
  printf '.env\n' > "$DEMO/clones/$app/.gitignore"
  git -C "$DEMO/clones/$app" add -A
  git -C "$DEMO/clones/$app" commit -q -m "Initial $app"
  git -C "$DEMO/clones/$app" push -q origin main
done

# A feature branch in web only: a cluster for it reuses the default api.
git -C "$DEMO/clones/web" checkout -q -b feature/hello
sed -i '' 's/^TITLE = .*/TITLE = "Grove demo — feature\/hello"/' "$DEMO/clones/web/server.py"
git -C "$DEMO/clones/web" commit -q -am "Change the title"
git -C "$DEMO/clones/web" push -q origin feature/hello
git -C "$DEMO/clones/web" checkout -q main

# A branch in both repos.
for app in api web; do
  git -C "$DEMO/clones/$app" checkout -q -b feature/both
  sed -i '' 's/^GREETING = .*/GREETING = "hello from feature\/both"/; s/^TITLE = .*/TITLE = "Grove demo — feature\/both"/' "$DEMO/clones/$app/server.py"
  git -C "$DEMO/clones/$app" commit -q -am "Change the text"
  git -C "$DEMO/clones/$app" push -q origin feature/both
  git -C "$DEMO/clones/$app" checkout -q main
done

cat > "$DEMO/workspace/grove.toml" <<TOML
name = "demo"
idle_timeout = "5m"
TOML

cat > "$DEMO/workspace/projects/api.toml" <<TOML
name = "api"
repo = "$DEMO/origin/api.git"
copy_from_original = [".env"]

[processes.server]
run = "python3 -u server.py"
port = { env = "PORT", default = 4001 }
ready = { http = "/health", timeout = "20s" }
domain = true
TOML

cat > "$DEMO/workspace/projects/web.toml" <<TOML
name = "web"
repo = "$DEMO/origin/web.git"
copy_from_original = [".env"]

[env]
API_URL = "{{ projects.api.server.url }}"
API_LOCAL_URL = "{{ projects.api.server.local_url }}"

[processes.app]
run = "python3 -u server.py"
port = { env = "PORT", default = 4002 }
ready = { http = "/", timeout = "20s" }
domain = { wildcard = true }
depends_on = ["api.server"]
TOML

cat > "$DEMO/home/config.toml" <<TOML
workspace = "$DEMO/workspace"
clones_root = "$DEMO/clones"
port_range = [4100, 4199]
TOML

echo "Demo ready in $DEMO"
echo "  GROVE_HOME=$DEMO/home grove headless --https-port 8443"

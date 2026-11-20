#!/usr/bin/env bash
# `make app NAME=foo`: a new app, apps/foo, copied from apps/template
# (#206; ARCHITECTURE section 8, "Building and bundling"). apps/ takes
# every directory as a crate and `make build` bundles every cdylib in it,
# and the launcher lists each bundled app whose manifest grants `desktop`,
# so the copy needs no registering: its crate is renamed, nothing else.
set -euo pipefail

name="${1:-}"
root="$(cd "$(dirname "$0")/.." && pwd)"
apps="$root/apps"

fail() {
	echo "make app: $*" >&2
	exit 2
}

[ -n "$name" ] || fail "usage: make app NAME=<name> (lower case letters, digits, - and _; starting with a letter)"
# The kernel's build script takes these stems only (kernel/build.rs).
[[ "$name" =~ ^[a-z][a-z0-9_-]*$ ]] || fail "$name: lower case letters, digits, - and _ only, starting with a letter"
case "$name" in
test-*) fail "$name: test- names are the runtime's test apps" ;;
template | lumen-app | testkit) fail "$name: that name is taken (apps/$name)" ;;
esac
[ ! -e "$apps/$name" ] || fail "apps/$name already exists"

cp -R "$apps/template" "$apps/$name"
# The package's name is the crate's (and the module's) name.
manifest="$apps/$name/Cargo.toml"
sed -e "s/^name = \"template\"$/name = \"$name\"/" \
	-e "s/^description = .*/description = \"The $name app (made by \`make app NAME=$name\` from apps\/template).\"/" \
	"$manifest" >"$manifest.new"
mv "$manifest.new" "$manifest"
grep -q "^name = \"$name\"$" "$manifest" || fail "apps/$name/Cargo.toml: couldn't rename its package"

echo "make app: apps/$name made from apps/template; \`make build\` builds and bundles it (\`run $name\`, and the launcher)"

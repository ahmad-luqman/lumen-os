#!/usr/bin/env bash
# `make app NAME=foo`: a new app, apps/foo, copied from apps/template
# (#206; ARCHITECTURE section 8, "Building and bundling"). apps/ takes
# every directory but dot ones as a crate, `make build` bundles every
# cdylib in it, and the launcher lists each bundled app whose manifest
# grants `desktop`: the copy needs no registering, only its crate renamed.
#
# Refused, making nothing:
# - a name kernel/build.rs wouldn't embed (lower case letters, digits, `-`
#   and `_`), or one not starting with a letter (this script's own rule: a
#   leading `-` or `_` breaks cargo for the whole workspace);
# - a test app's name: `test-`/`test_` names, or a test app's module name
#   (apps/test-hello is `hello`; kernel/build.rs refuses a name in both
#   sets);
# - a name the apps/ workspace has already: a directory, a path
#   dependency (`lumen-app-api`, `lumen-heap`), or the same crate once `-`
#   and `_` are alike (cargo's output file is named with `_`: `a-b` and
#   `a_b` would overwrite each other's module).
# The copy is made in a dot directory (not a member) and moved into place
# only once it is finished, so a failure leaves nothing behind.
set -euo pipefail

name="${1:-}"
root="$(cd "$(dirname "$0")/.." && pwd)"
apps="$root/apps"

fail() {
	echo "make app: $*" >&2
	exit 2
}

# A crate's name as cargo names its output: `-` as `_`.
crate_file() {
	echo "${1//-/_}"
}

[ -n "$name" ] || fail "usage: make app NAME=<name> (lower case letters, digits, - and _; starting with a letter)"
[[ "$name" =~ ^[a-z][a-z0-9_-]*$ ]] || fail "$name: lower case letters, digits, - and _ only, starting with a letter"
case "$name" in
test-* | test_*) fail "$name: test- names are the runtime's test apps" ;;
esac
[ ! -e "$apps/$name" ] || fail "apps/$name already exists"

want="$(crate_file "$name")"
for dir in "$apps"/*/; do
	have="$(basename "$dir")"
	case "$have" in
	test-*)
		# A test app's module drops its `test-`.
		[ "$(crate_file "${have#test-}")" != "$want" ] || fail "$name: a test app's name (apps/$have)"
		;;
	*)
		[ "$(crate_file "$have")" != "$want" ] || fail "$name: that name is taken (apps/$have)"
		;;
	esac
done
# The workspace's path dependencies (apps/Cargo.toml): a crate named so
# would collide with one in the lock file.
while IFS= read -r dep; do
	[ "$(crate_file "$dep")" != "$want" ] || fail "$name: that name is taken (a dependency of apps/)"
done < <(sed -n 's/^\([a-z0-9_-]*\) = { path = .*/\1/p' "$apps/Cargo.toml")

tmp="$(mktemp -d "$apps/.new-$name.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
cp -R "$apps/template/." "$tmp/"
# The package's name is the crate's (and the module's) name.
manifest="$tmp/Cargo.toml"
sed -e "s/^name = \"template\"$/name = \"$name\"/" \
	-e "s/^description = .*/description = \"The $name app (made by \`make app NAME=$name\` from apps\/template).\"/" \
	"$manifest" >"$manifest.new"
mv "$manifest.new" "$manifest"
grep -q "^name = \"$name\"$" "$manifest" || fail "apps/$name/Cargo.toml: couldn't rename its package"
mv "$tmp" "$apps/$name"
trap - EXIT

echo "make app: apps/$name made from apps/template; \`make build\` builds and bundles it (\`run $name\`, and the launcher)"

#!/usr/bin/env bash
set -euo pipefail

# bump the patch version in Cargo.toml, commit, tag, and push

CARGO="Cargo.toml"
current=$(grep '^version = ' "$CARGO" | head -1 | sed 's/version = "\(.*\)"/\1/')
IFS='.' read -r major minor patch <<< "$current"
next="$major.$minor.$((patch + 1))"

if [[ "$OSTYPE" == "darwin"* ]]; then
  sed -i '' "s/^version = \"$current\"/version = \"$next\"/" "$CARGO"
else
  sed -i "s/^version = \"$current\"/version = \"$next\"/" "$CARGO"
fi
cargo build --quiet 2>/dev/null

git add -A
git commit -m "chore: bump version to $next"
git tag "v$next"
git push origin master --tags

echo "released v$next"

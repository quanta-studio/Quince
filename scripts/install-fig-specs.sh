#!/bin/sh
# Downloads the Fig completion specs into the directory quince reads them from.
#
#   scripts/install-fig-specs.sh [destination]
#
# Needs curl, tar and shasum. The specs are JavaScript modules, exactly as Fig published them.
# Nothing here runs them: quince does, inside its own engine, where they can reach
# nothing but the commands their generators ask for.
set -eu

version=2.692.3
sha256=b07159ef0811bc8cec28e3a3016e6ba9568d774b75b8333dc6a5f2acf7d2c3af
dest=${1:-${XDG_DATA_HOME:-$HOME/.local/share}/quince/specs}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL -o "$work/specs.tgz" "https://registry.npmjs.org/@withfig/autocomplete/-/autocomplete-$version.tgz"
# The package is pinned by content, so what gets installed is what was tested.
echo "$sha256  $work/specs.tgz" | shasum -a 256 -c - >/dev/null

mkdir -p "$dest"
tar -xzf "$work/specs.tgz" -C "$dest" --strip-components 2 package/build
tar -xzf "$work/specs.tgz" -C "$dest" --strip-components 1 package/LICENSE
echo "installed $(find "$dest" -name '*.js' | wc -l | tr -d ' ') spec files into $dest"

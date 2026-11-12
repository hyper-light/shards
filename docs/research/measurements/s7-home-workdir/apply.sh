# Lays the carrier's files/ over a clean checkout of $1 in ~/g121-tmp/s7p2/shards, each
# checked by sha256 against files.sha256, then prints the overlay's paths.
set -eu
rev=$1
src=$PWD
cd ~/g121-tmp/s7p2/shards
git checkout -q -- .
git clean -fdq
git cat-file -e "$rev^{commit}" 2>/dev/null || git fetch -q origin main
git checkout -q --detach "$rev"
if [ -f "$src/files.sha256" ]; then
  (cd "$src/files" && sha256sum -c --quiet "$src/files.sha256")
  (cd "$src/files" && find . -type f | sed 's|^\./||') | while read -r p; do
    mkdir -p "$(dirname "$p")"
    cp "$src/files/$p" "$p"
    echo "overlay $p"
  done
fi
git status --short | head -40

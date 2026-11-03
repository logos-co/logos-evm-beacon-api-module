#!/usr/bin/env bash
#
# Run this repo's doc-tests end-to-end and regenerate their Markdown into
# ./outputs/. The runner is the shared `doctest` CLI
# (https://github.com/logos-co/logos-doctest), invoked via its flake.
#
# Override the runner with DOCTEST, e.g. to use a local checkout:
#   DOCTEST="nix run path:../../logos-doctest --" ./run.sh
#
set -euo pipefail
cd "$(dirname "$0")"

read -r -a DOCTEST <<< "${DOCTEST:-nix run github:logos-co/logos-doctest --}"

if [ -e outputs ]; then chmod -R u+w outputs 2>/dev/null || true; fi
rm -rf outputs && mkdir -p outputs

for spec in *.test.yaml; do
  name="$(basename "${spec%.test.yaml}")"
  echo "==> Running ${spec}"
  "${DOCTEST[@]}" run "${spec}" --verbose --output-dir ./outputs/
  echo "==> Generating outputs/${name}.md"
  "${DOCTEST[@]}" generate "${spec}" -o "outputs/${name}.md"
done

echo "==> Cleaning build artifacts from outputs/ (keeps .md and images/)"
"${DOCTEST[@]}" clean ./outputs --verbose 2>/dev/null || true
echo "==> Done."

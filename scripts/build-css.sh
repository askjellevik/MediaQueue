#!/usr/bin/env bash
# Compiles styles/main.css (Tailwind plus styles/base.css) into
# web/css/app.css. Needs the Tailwind v4 standalone CLI: download
# `tailwindcss-linux-<arch>` from
# https://github.com/tailwindlabs/tailwindcss/releases, check it against
# sha256sums.txt, and either put it on PATH as `tailwindcss` or point
# TAILWIND at it. Run after changing styles/ or any class name in web/.
set -euo pipefail
cd "$(dirname "$0")/.."
"${TAILWIND:-tailwindcss}" --input styles/main.css --output web/css/app.css --minify

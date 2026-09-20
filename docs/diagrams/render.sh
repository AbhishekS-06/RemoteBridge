#!/bin/sh
# Regenerate the PNG next to each .mmd file. GitHub's built-in Mermaid
# rendering adds zoom/pan controls that cover the diagram, so the README
# embeds these static images instead. Run after editing any .mmd file:
#
#   sh docs/diagrams/render.sh
#
# Needs Node and a local Chrome. Uses the system Chrome (see
# puppeteer-config.json) so no browser download is required.
set -e
cd "$(dirname "$0")"
export PUPPETEER_SKIP_DOWNLOAD=1
for src in *.mmd; do
  npx --yes @mermaid-js/mermaid-cli \
    -i "$src" -o "${src%.mmd}.png" \
    -c mermaid-config.json -p puppeteer-config.json \
    -b white -s 3
done

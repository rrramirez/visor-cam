#!/usr/bin/env bash
# Deploys site/ to Cloudflare Pages (https://visor-cam.pages.dev).
#
#   scripts/deploy-site.sh
#
# Needs a Wrangler login with the pages:write scope
# (`npx wrangler@4 login --scopes account:read pages:write`), or
# CLOUDFLARE_API_TOKEN (Account > Cloudflare Pages > Edit) and CLOUDFLARE_ACCOUNT_ID.
set -euo pipefail
cd "$(dirname "$0")/.."
npx --yes wrangler@4 pages deploy site --project-name visor-cam --branch main --commit-dirty=true

#!/usr/bin/env bash
# Create (or reuse) the TokenTrimmer tier products + monthly prices in Stripe
# TEST mode, and print the price IDs to paste into env.
#
#   STRIPE_SECRET_KEY=sk_test_... ./scripts/setup-stripe-test-prices.sh
#
# Guards on sk_test_ so it can never touch a live account. Idempotent: it
# looks up an existing product by exact name and reuses it (and an existing
# active monthly price at the same amount) instead of creating duplicates,
# so re-running is safe.
set -euo pipefail

: "${STRIPE_SECRET_KEY:?must be set; export it or source your env file}"
case "$STRIPE_SECRET_KEY" in
  sk_test_*) ;;
  *) echo "Refusing: key does not start with sk_test_. Switch to test mode." >&2; exit 1 ;;
esac

for bin in curl jq; do
  command -v "$bin" >/dev/null 2>&1 || { echo "missing dependency: $bin" >&2; exit 1; }
done

API=https://api.stripe.com/v1

# Find an existing product by exact name, or create one. Echoes the product id.
find_or_create_product() {
  local name="$1" desc="$2" existing
  existing=$(curl -sS "${API}/products?active=true&limit=100" -u "${STRIPE_SECRET_KEY}:" \
    | jq -r --arg n "$name" '.data[] | select(.name == $n) | .id' | head -n1)
  if [ -n "$existing" ]; then
    echo "$existing"
    return
  fi
  curl -sS "${API}/products" -u "${STRIPE_SECRET_KEY}:" \
    --data-urlencode "name=${name}" \
    --data-urlencode "description=${desc}" \
    | jq -r '.id'
}

# Reuse an active price at the target amount + interval that already carries the
# right tier metadata, else create one. The webhook reads price.metadata.tier
# to map a subscription back to a tier (defaulting to "pro"), so Team/Scale
# MUST be stamped or they would silently resolve to Pro. Echoes the price id.
find_or_create_price() {
  local product_id="$1" cents="$2" tier="$3" interval="$4" existing
  existing=$(curl -sS "${API}/prices?product=${product_id}&active=true&limit=100" -u "${STRIPE_SECRET_KEY}:" \
    | jq -r --argjson c "$cents" --arg t "$tier" --arg i "$interval" \
        '.data[] | select(.unit_amount == $c and .currency == "usd" and .recurring.interval == $i and .metadata.tier == $t) | .id' \
    | head -n1)
  if [ -n "$existing" ]; then
    echo "$existing"
    return
  fi
  curl -sS "${API}/prices" -u "${STRIPE_SECRET_KEY}:" \
    -d "product=${product_id}" \
    -d "unit_amount=${cents}" \
    -d "currency=usd" \
    -d "recurring[interval]=${interval}" \
    -d "metadata[tier]=${tier}" \
    | jq -r '.id'
}

# Mint a tier's monthly + annual flat prices (annual = monthly × 10 → 2 months
# free). Echoes "<monthly_id> <annual_id>".
mint_tier() {
  local name="$1" monthly="$2" tier="$3" desc="$4" product m a
  product=$(find_or_create_product "$name" "$desc")
  m=$(find_or_create_price "$product" "$monthly" "$tier" month)
  a=$(find_or_create_price "$product" "$(( monthly * 10 ))" "$tier" year)
  printf '%s %s\n' "$m" "$a"
}

read -r PRO_M PRO_A < <(mint_tier "TokenTrimmer Pro" 9900 pro \
  "Up to 500K req/mo, 90d retention, CSV/JSON export, L1 + L2 cache")
read -r TEAM_M TEAM_A < <(mint_tier "TokenTrimmer Team" 39900 team \
  "Pro features + RBAC, SSO (Google + GitHub), PR bot on up to 10 repos, unlimited seats")
read -r SCALE_M SCALE_A < <(mint_tier "TokenTrimmer Scale" 149900 scale \
  "Team features + S3 Object Lock audit, signed monthly SLO PDFs, email support")

cat <<EOF

# Paste into .env.development (and Fly secrets for prod with live-mode IDs later):
STRIPE_PRICE_PRO=${PRO_M}
STRIPE_PRICE_TEAM=${TEAM_M}
STRIPE_PRICE_SCALE=${SCALE_M}
STRIPE_PRICE_PRO_ANNUAL=${PRO_A}
STRIPE_PRICE_TEAM_ANNUAL=${TEAM_A}
STRIPE_PRICE_SCALE_ANNUAL=${SCALE_A}
EOF

#!/usr/bin/env bash
# Create (or reuse) the TokenTrimmer overage billing meter + per-tier metered
# prices in Stripe TEST mode, and print the price IDs to paste into env.
#
#   STRIPE_SECRET_KEY=sk_test_... ./scripts/setup-stripe-overage.sh
#
# Run AFTER setup-stripe-test-prices.sh (it attaches the metered prices to the
# same TokenTrimmer Pro/Team/Scale products). Guards on sk_test_. Idempotent:
# reuses an existing meter (by event_name) and an existing overage price (by
# product + metadata) instead of creating duplicates.
#
# The gateway reports usage via event_name=tt_overage_requests with
# payload{stripe_customer_id, value}; the rates below are the published
# per-10K overage ($2.50 / $1.50 / $1.00 → per-request cents).
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
EVENT_NAME=tt_overage_requests

# Meter: find by event_name, else create.
meter_id=$(curl -sS "${API}/billing/meters?limit=100" -u "${STRIPE_SECRET_KEY}:" \
  | jq -r --arg e "$EVENT_NAME" '.data[] | select(.event_name==$e and .status=="active") | .id' | head -n1)
if [ -z "$meter_id" ]; then
  meter_id=$(curl -sS "${API}/billing/meters" -u "${STRIPE_SECRET_KEY}:" \
    --data-urlencode "display_name=TokenTrimmer overage requests" \
    -d "event_name=${EVENT_NAME}" \
    -d "default_aggregation[formula]=sum" \
    -d "customer_mapping[type]=by_id" \
    -d "customer_mapping[event_payload_key]=stripe_customer_id" \
    -d "value_settings[event_payload_key]=value" \
    | jq -r '.id')
fi

product_id_by_name() {
  curl -sS "${API}/products?active=true&limit=100" -u "${STRIPE_SECRET_KEY}:" \
    | jq -r --arg n "$1" '.data[] | select(.name==$n) | .id' | head -n1
}

# Reuse an existing metered overage price on the product (by tier metadata),
# else create one tied to the meter at the per-request rate. Echoes the id.
overage_price() {
  local product_name="$1" tier="$2" decimal="$3" pid existing
  pid=$(product_id_by_name "$product_name")
  if [ -z "$pid" ]; then echo "PRODUCT NOT FOUND: $product_name" >&2; return 1; fi
  existing=$(curl -sS "${API}/prices?product=${pid}&active=true&limit=100" -u "${STRIPE_SECRET_KEY}:" \
    | jq -r --arg t "$tier" \
        '.data[] | select(.recurring.usage_type=="metered" and .metadata.tier==$t and .metadata.kind=="overage") | .id' \
    | head -n1)
  if [ -n "$existing" ]; then echo "$existing"; return; fi
  curl -sS "${API}/prices" -u "${STRIPE_SECRET_KEY}:" \
    -d "product=${pid}" \
    -d "currency=usd" \
    -d "recurring[interval]=month" \
    -d "recurring[usage_type]=metered" \
    -d "recurring[meter]=${meter_id}" \
    -d "billing_scheme=per_unit" \
    -d "unit_amount_decimal=${decimal}" \
    -d "metadata[tier]=${tier}" \
    -d "metadata[kind]=overage" \
    | jq -r '.id'
}

PRO=$(overage_price "TokenTrimmer Pro" pro 0.025)     # $2.50 / 10K
TEAM=$(overage_price "TokenTrimmer Team" team 0.015)  # $1.50 / 10K
SCALE=$(overage_price "TokenTrimmer Scale" scale 0.010) # $1.00 / 10K

cat <<EOF

# Stripe overage meter + metered prices (TEST). meter event_name=${EVENT_NAME}
# meter id (informational): ${meter_id}
# Paste into .env.development:
STRIPE_OVERAGE_PRICE_PRO=${PRO}
STRIPE_OVERAGE_PRICE_TEAM=${TEAM}
STRIPE_OVERAGE_PRICE_SCALE=${SCALE}
EOF

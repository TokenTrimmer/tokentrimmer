#!/usr/bin/env bash
# End-to-end validation of metered overage billing using a Stripe TEST CLOCK.
#
#   STRIPE_SECRET_KEY=sk_test_... \
#   STRIPE_PRICE_PRO=price_... STRIPE_OVERAGE_PRICE_PRO=price_... \
#   ./scripts/validate-stripe-overage.sh
#
# Flow: create a test clock + customer (test card) → subscribe to the flat Pro
# price + the metered overage price → report 100,000 overage requests to the
# meter → advance the clock past the period end → read the invoice and assert a
# ~$25 overage line (100,000 × $0.00025). Cleans up the test clock (and the
# customer/subscription it owns) at the end. TEST mode only.
set -euo pipefail

: "${STRIPE_SECRET_KEY:?must be set}"
case "$STRIPE_SECRET_KEY" in
  sk_test_*) ;;
  *) echo "Refusing: not a test key." >&2; exit 1 ;;
esac
: "${STRIPE_PRICE_PRO:?set the flat Pro price id}"
: "${STRIPE_OVERAGE_PRICE_PRO:?set the metered overage price id}"
for b in curl jq; do command -v "$b" >/dev/null 2>&1 || { echo "missing $b" >&2; exit 1; }; done

API=https://api.stripe.com/v1
auth() { curl -sS "$@" -u "${STRIPE_SECRET_KEY}:"; }

cleanup() { [ -n "${CLOCK:-}" ] && auth -X DELETE "$API/test_helpers/test_clocks/$CLOCK" >/dev/null 2>&1 || true; }
trap cleanup EXIT

NOW=$(date +%s)
echo "1) create test clock @ $NOW"
CLOCK=$(auth "$API/test_helpers/test_clocks" -d "frozen_time=$NOW" -d "name=overage-validation" | jq -r '.id')
[ "$CLOCK" != "null" ] && [ -n "$CLOCK" ] || { echo "FAILED to create clock"; exit 1; }
echo "   clock=$CLOCK"

echo "2) customer on the clock"
CUS=$(auth "$API/customers" -d "test_clock=$CLOCK" -d "name=Overage Validation" \
  -d "email=overage-validation@example.com" | jq -r '.id')
echo "   customer=$CUS"

echo "3) subscription: flat Pro + metered overage (billed via send_invoice — no card needed)"
SUB_JSON=$(auth "$API/subscriptions" \
  -d "customer=$CUS" \
  -d "items[0][price]=$STRIPE_PRICE_PRO" \
  -d "items[1][price]=$STRIPE_OVERAGE_PRICE_PRO" \
  -d "collection_method=send_invoice" \
  -d "days_until_due=30" \
  -d "metadata[org_id]=00000000-0000-0000-0000-000000000000")
SUB=$(echo "$SUB_JSON" | jq -r '.id')
# Newer API versions expose the period at the item level, not the subscription.
PERIOD_END=$(echo "$SUB_JSON" | jq -r '.current_period_end // .items.data[0].current_period_end // empty')
echo "   sub=$SUB  status=$(echo "$SUB_JSON" | jq -r '.status')  period_end=$PERIOD_END"
[ "$SUB" != "null" ] && [ -n "$SUB" ] || { echo "FAILED to create subscription: $SUB_JSON"; exit 1; }
[ -n "$PERIOD_END" ] || { echo "FAILED: no current_period_end on subscription"; exit 1; }

echo "4) report 100,000 overage requests to the meter"
auth "$API/billing/meter_events" \
  -d "event_name=tt_overage_requests" \
  -d "identifier=validation-$NOW" \
  -d "timestamp=$NOW" \
  -d "payload[stripe_customer_id]=$CUS" \
  -d "payload[value]=100000" | jq '{event_name, identifier, ts: .timestamp}'
echo "   (meter events ingest asynchronously)"
sleep 8

echo "5) advance the clock past the period end"
ADV_TO=$((PERIOD_END + 86400))
auth "$API/test_helpers/test_clocks/$CLOCK/advance" -d "frozen_time=$ADV_TO" >/dev/null
for _ in $(seq 1 40); do
  ST=$(auth "$API/test_helpers/test_clocks/$CLOCK" | jq -r '.status')
  echo "   clock status: $ST"
  [ "$ST" = "ready" ] && break
  sleep 5
done

echo "6) invoices for the customer"
INVOICES=$(auth "$API/invoices?customer=$CUS&limit=10")
echo "$INVOICES" | jq '[.data[] | {number, status, total,
  lines: [.lines.data[] | {desc: .description, amount,
    price: (.price.id // .pricing.price_details.price)}]}]'

echo "7) verdict"
OVERAGE_CENTS=$(echo "$INVOICES" | jq --arg p "$STRIPE_OVERAGE_PRICE_PRO" \
  '[.data[].lines.data[] | select((.price.id // .pricing.price_details.price)==$p) | .amount] | add // 0')
echo "   overage billed: ${OVERAGE_CENTS} cents (expected 2500 = \$25.00)"
if [ "$OVERAGE_CENTS" = "2500" ]; then
  echo "   ✅ PASS — metered overage billed correctly"
else
  echo "   ⚠️ MISMATCH — got ${OVERAGE_CENTS}, expected 2500 (see invoice lines above)"
fi

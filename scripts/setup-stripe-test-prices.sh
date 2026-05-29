  #!/usr/bin/env bash 
  set -euo pipefail

  : "${STRIPE_SECRET_KEY:?must be set; export it or source your env file}"
  case "$STRIPE_SECRET_KEY" in
    sk_test_*) ;;     
    *) echo "Refusing: key does not start with sk_test_. Switch to test mode." >&2; exit 1 ;;
  esac
  
  create_price() {    
    local name="$1"
    local cents="$2"
    local desc="$3"
    local product_id
    product_id=$(curl -sS https://api.stripe.com/v1/products \
      -u "${STRIPE_SECRET_KEY}:" \
      -d "name=${name}" \
      -d "description=${desc}" \
      | jq -r '.id')  
    local price_id
    price_id=$(curl -sS https://api.stripe.com/v1/prices \
      -u "${STRIPE_SECRET_KEY}:" \
      -d "product=${product_id}" \
      -d "unit_amount=${cents}" \
      -d "currency=usd" \
      -d "recurring[interval]=month" \
      | jq -r '.id')
    echo "$price_id"
  }

  PRO=$(create_price "TokenTrimmer Pro" 9900 "Up to 500K req/mo, 90d retention, CSV/JSON export, L1 + L2 cache")
  TEAM=$(create_price "TokenTrimmer Team" 39900 "Pro features + RBAC, SSO (Google + GitHub), PR bot on up to 10 
  repos, 25 seats")
  SCALE=$(create_price "TokenTrimmer Scale" 149900 "Team features + S3 Object Lock audit, signed monthly SLO 
  PDFs, dedicated support")

  cat <<EOF

  # Paste into env.development (and Fly secrets for prod with live-mode IDs later):
  STRIPE_PRICE_PRO=${PRO}
  STRIPE_PRICE_TEAM=${TEAM}
  STRIPE_PRICE_SCALE=${SCALE}
  EOF
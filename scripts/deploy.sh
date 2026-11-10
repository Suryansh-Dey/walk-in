#!/usr/bin/env bash
# Builds both Lambda functions and deploys the stack.
# Needs cargo-lambda, the AWS CLI with a logged-in session, and the variables checked below.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${OPENROUTER_API_KEY:?Set OPENROUTER_API_KEY}"
: "${PAYPAL_CLIENT_ID:?Set PAYPAL_CLIENT_ID}"
: "${PAYPAL_CLIENT_SECRET:?Set PAYPAL_CLIENT_SECRET}"

STACK="${STACK:-walk-in}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
ACCOUNT="$(aws sts get-caller-identity --query Account --output text)"
BUCKET="walk-in-artifacts-$ACCOUNT-$AWS_REGION"

aws s3api head-bucket --bucket "$BUCKET" 2>/dev/null || aws s3 mb "s3://$BUCKET" >/dev/null

cargo lambda build --release --arm64 --output-format zip

# Keys carry the content hash, so CloudFormation only updates a function whose code changed.
upload() {
  local zip="target/lambda/$1/bootstrap.zip"
  local key="$1-$(sha256sum "$zip" | cut -c1-16).zip"
  aws s3 cp --only-show-errors "$zip" "s3://$BUCKET/$key"
  echo "$key"
}
WEB_KEY="$(upload web)"
IMPORTER_KEY="$(upload importer)"

# AWS requires 10 unreserved executions to remain, which new accounts can't spare.
UNRESERVED="$(aws lambda get-account-settings --query AccountLimit.UnreservedConcurrentExecutions --output text)"
CONCURRENCY=20
if (( UNRESERVED < 30 )); then
  echo "This account's concurrency limit is low, so the web function won't reserve any."
  CONCURRENCY=0
fi

aws cloudformation deploy \
  --stack-name "$STACK" \
  --template-file infra/template.yaml \
  --capabilities CAPABILITY_IAM \
  --no-fail-on-empty-changeset \
  --parameter-overrides \
    "ArtifactBucket=$BUCKET" \
    "WebArtifactKey=$WEB_KEY" \
    "ImporterArtifactKey=$IMPORTER_KEY" \
    "OpenRouterApiKey=$OPENROUTER_API_KEY" \
    "PayPalClientId=$PAYPAL_CLIENT_ID" \
    "PayPalClientSecret=$PAYPAL_CLIENT_SECRET" \
    "PayPalWebhookId=${PAYPAL_WEBHOOK_ID:-}" \
    "DemoMerchantEmail=${DEMO_MERCHANT_EMAIL:-}" \
    "AllowUnverifiedPublish=${ALLOW_UNVERIFIED_PUBLISH:-0}" \
    "WebConcurrency=$CONCURRENCY"

aws cloudformation describe-stacks --stack-name "$STACK" --query 'Stacks[0].Outputs' --output table

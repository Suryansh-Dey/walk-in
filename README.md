# Walk-In

AI shoppers can't find most small shops. Walk-In lets them walk in.

A shop owner pastes their website address. Walk-In reads the site, turns it into a checked catalog, and gives the shop its own [MCP](https://modelcontextprotocol.io) server. Any AI assistant can then browse that shop and start an order. The shopper approves the order in PayPal, and the money goes to the shop's own PayPal account.

The owner also sees something their website could never tell them: what people asked for that the shop doesn't sell.

## How it works

1. **Read.** A crawler reads up to 14 pages of the site, plus up to four linked PDFs, since many small food businesses publish their menu only as a PDF. It respects `robots.txt` and refuses private addresses. Text hidden with CSS is removed before the model sees it, so a page can't quietly instruct the model.
2. **Extract and verify.** DeepSeek returns a catalog as strict structured output. Code then checks every price: its exact text must appear on the cited page and parse to the claimed amount. Anything else is flagged for the owner. Items with no published price are kept as "by quote", and the model is told never to estimate one.
3. **Open.** The owner fixes what was flagged, connects PayPal, proves they control the site with a meta tag, and opens the shop.
4. **Sell.** Each shop gets `/mcp/<slug>` with seven tools, and a hosted chat at `/shop/<slug>` for people without an MCP client. Both call the same code.
5. **Get paid.** `create_checkout` builds the order only from catalog prices, so an assistant can't change a total. The shopper approves in PayPal and the order is captured once, whether the return page, the assistant or a webhook reports it first.
6. **Quote.** Custom work and items with no listed price go through `request_quote`. The request lands on the owner's dashboard, the owner names a price, and the shopper pays it through PayPal from a status link. Owners see requests on the dashboard only, there is no email yet. The PayPal order is created when the shopper clicks Pay, because PayPal drops unapproved orders within hours and a shopper may come back days later.
7. **Learn.** The dashboard shows live activity, orders with tracking, and the unmet demand assistants reported.

## How PayPal is used

| API | Purpose |
| --- | --- |
| Orders v2: create | One order per checkout, with `purchase_units[].payee` set to the shop, so funds go straight to the seller |
| Orders v2: get, capture | Confirm approval and capture idempotently with `PayPal-Request-Id` |
| Webhooks | Verify signatures, then apply approvals, captures and refunds |

Walk-In never holds the money and has no access to the shop's PayPal account. PayPal only lets the receiving account refund a payment, so the dashboard links each order to the shop's own PayPal, and the refund webhook brings the result back. Tracking numbers are kept in Walk-In and shared with the shopper's assistant.

Amounts are integer minor units throughout, with PayPal's zero-decimal currencies handled.

## Architecture

Rust on AWS Lambda behind a Function URL, with one DynamoDB table.

- `web` is an axum app serving the API, the MCP endpoints (`rmcp`, stateless JSON mode), and the embedded frontend.
- `importer` is a worker invoked asynchronously, because reading a site takes longer than a request should.
- Calls to the model go through [`open-client`](https://crates.io/crates/open-client) to OpenRouter.

## Run it locally

You need Rust, Node for the local DynamoDB, and an OpenRouter key.

```sh
npx -y dynalite --port 8765 &
cp .env.example .env   # fill in the keys, and set DYNAMODB_ENDPOINT=http://localhost:8765
set -a; source .env; set +a
AWS_ACCESS_KEY_ID=local AWS_SECRET_ACCESS_KEY=local AWS_REGION=us-east-1 cargo run
```

Open http://localhost:3000 and pick a sample shop.

To connect an assistant, open a shop and copy the command from its dashboard:

```sh
claude mcp add --transport http <slug> https://<your-host>/mcp/<slug>
```

## Tests

```sh
cargo test
```

The end-to-end test walks a shop from import to refund against mocked PayPal and OpenRouter, using a local DynamoDB. It checks that an assistant can't set a price, that hidden text never reaches the model, that a forged webhook is rejected, and that an order is captured exactly once.

```sh
npx -y dynalite --port 8765 &
DYNAMODB_ENDPOINT=http://localhost:8765 AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x AWS_REGION=us-east-1 \
  cargo test --test flow -- --ignored
```

## Deploy

You need [cargo-lambda](https://www.cargo-lambda.info) and a logged-in AWS CLI.

```sh
export OPENROUTER_API_KEY=... PAYPAL_CLIENT_ID=... PAYPAL_CLIENT_SECRET=...
export DEMO_MERCHANT_EMAIL=your-sandbox-business-account@example.com ALLOW_UNVERIFIED_PUBLISH=1
./scripts/deploy.sh
```

The script prints the app URL and a webhook URL. Register the webhook in your PayPal app for `CHECKOUT.ORDER.APPROVED`, `PAYMENT.CAPTURE.COMPLETED` and `PAYMENT.CAPTURE.REFUNDED`, then run the script again with `PAYPAL_WEBHOOK_ID` set.

`ALLOW_UNVERIFIED_PUBLISH=1` lets anyone open a shop without proving they own the site. Use it only for sandbox demos.

## AI tools used

- **DeepSeek V4.1 Flash**, through OpenRouter, at runtime: catalog extraction and the hosted shopping assistant.
- **Claude Code** (Claude Opus 5.5) during development.

## License

MIT

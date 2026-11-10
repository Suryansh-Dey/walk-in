//! Walks a shop from import to refund against mocked PayPal and OpenRouter.
//!
//! Needs a local DynamoDB, so it is ignored by default:
//!
//! ```sh
//! npx -y dynalite --port 8765 &
//! DYNAMODB_ENDPOINT=http://localhost:8765 AWS_ACCESS_KEY_ID=x AWS_SECRET_ACCESS_KEY=x AWS_REGION=us-east-1 \
//!   cargo test --test flow -- --ignored
//! ```

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use walk_in::App;
use walk_in::config::Config;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SELLER: &str = "seller@example.com";

fn completion(message: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "choices": [{ "message": message, "finish_reason": "stop" }] }))
}

fn catalog(base: &str) -> Value {
    let product = |name: &str, price: f64, price_text: &str| {
        json!({
            "name": name, "description": "From the sample site.", "price": price, "price_text": price_text,
            "category": "Cakes", "tags": ["birthday"], "options": [], "image_url": null,
            "source_url": format!("{base}/demo/juniper-bakery/cakes.html"), "in_stock": true,
        })
    };
    json!({
        "shop": {
            "name": "Juniper & Rye Bakery", "description": "A bakery.", "location": "Portland", "contact": "", "hours": "",
            "currency": "USD", "ships": false, "pickup": true, "shipping_fee": null, "shipping_policy": "", "returns_policy": "",
        },
        "products": [
            product("Chocolate Celebration Cake, 6 inch", 34.0, "$34.00"),
            product("Wedding Tasting Box", 45.0, "forty-five dollars"),
        ],
        "warnings": [],
    })
}

async fn mock_paypal(paypal: &MockServer) {
    let reply = |status: u16, body: Value| ResponseTemplate::new(status).set_body_json(body);
    let paid = json!({
        "id": "PP1", "status": "COMPLETED",
        "payer": { "name": { "given_name": "Sam", "surname": "Buyer" }, "email_address": "buyer@example.com" },
        "purchase_units": [{ "payments": { "captures": [{ "id": "CAP1", "status": "COMPLETED" }] } }],
    });
    Mock::given(path("/v1/oauth2/token")).respond_with(reply(200, json!({ "access_token": "t", "expires_in": 3600 }))).mount(paypal).await;
    Mock::given(method("POST")).and(path("/v2/checkout/orders"))
        .respond_with(reply(201, json!({ "id": "PP1", "status": "PAYER_ACTION_REQUIRED", "links": [{ "rel": "payer-action", "href": "https://paypal.test/approve" }] })))
        .mount(paypal).await;
    Mock::given(method("GET")).and(path("/v2/checkout/orders/PP1")).respond_with(reply(200, json!({ "id": "PP1", "status": "APPROVED" }))).mount(paypal).await;
    // Capturing twice would charge twice, so this mock also proves it happens once.
    Mock::given(method("POST")).and(path("/v2/checkout/orders/PP1/capture")).respond_with(reply(201, paid)).expect(1).mount(paypal).await;
    Mock::given(path("/v1/notifications/verify-webhook-signature")).and(body_string_contains("forged"))
        .respond_with(reply(200, json!({ "verification_status": "FAILURE" }))).with_priority(1).mount(paypal).await;
    Mock::given(path("/v1/notifications/verify-webhook-signature"))
        .respond_with(reply(200, json!({ "verification_status": "SUCCESS" }))).with_priority(2).mount(paypal).await;
}

async fn start(paypal: &MockServer, llm: &MockServer) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let config = Config {
        port: 0,
        public_base_url: Some(base.clone()),
        // A fresh table per run, so stored rate limits can't leak between runs.
        table_name: format!("walk-in-test-{}", std::process::id()),
        dynamodb_endpoint: Some(std::env::var("DYNAMODB_ENDPOINT").expect("DYNAMODB_ENDPOINT must point at a local DynamoDB")),
        importer_function: None,
        openrouter_api_key: "test".into(),
        openrouter_model: "test/model".into(),
        openrouter_api_base: llm.uri(),
        paypal_api_base: paypal.uri(),
        paypal_client_id: "id".into(),
        paypal_client_secret: "secret".into(),
        paypal_webhook_id: "WH".into(),
        demo_merchant_email: SELLER.into(),
        allow_private_fetch: true,
        allow_unverified_publish: false,
    };
    let app = App::new(config).await.unwrap();
    app.store.ensure_local_table().await.unwrap();
    tokio::spawn(async move { axum::serve(listener, walk_in::web::router(app)).await });
    base
}

struct Owner {
    http: Client,
    base: String,
    slug: String,
    key: String,
}

impl Owner {
    async fn send(&self, verb: reqwest::Method, suffix: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self.http.request(verb, format!("{}/api/shops/{}{suffix}", self.base, self.slug)).header("x-owner-key", &self.key);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, suffix: &str) -> Value {
        self.send(reqwest::Method::GET, suffix, None).await.1
    }

    async fn publish(&self) -> (u16, Value) {
        self.send(reqwest::Method::POST, "/publish", None).await
    }
}

async fn import_sample(http: &Client, base: &str) -> Owner {
    let created: Value = http
        .post(format!("{base}/api/shops"))
        .json(&json!({ "url": format!("{base}/demo/juniper-bakery/index.html") }))
        .send().await.unwrap().json().await.unwrap();
    let owner = Owner { http: http.clone(), base: base.to_owned(), slug: created["slug"].as_str().unwrap().to_owned(), key: created["key"].as_str().unwrap().to_owned() };
    for _ in 0..100 {
        if owner.get("").await["status"] == "review" {
            return owner;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("import did not finish: {}", owner.get("").await);
}

struct McpClient {
    http: Client,
    url: String,
}

impl McpClient {
    async fn rpc(&self, method: &str, params: Value) -> reqwest::Response {
        self.http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send().await.unwrap()
    }

    async fn call(&self, tool: &str, arguments: Value) -> (bool, Value) {
        let response: Value = self.rpc("tools/call", json!({ "name": tool, "arguments": arguments })).await.json().await.unwrap();
        let text = response["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text in {response}"));
        (response["result"]["isError"] == json!(true), serde_json::from_str(text).unwrap_or_else(|_| json!(text)))
    }
}

fn bodies(requests: &[Request], wanted: &str) -> Vec<Value> {
    requests.iter().filter(|request| request.url.path() == wanted).map(|request| request.body_json().unwrap()).collect()
}

#[tokio::test]
#[ignore = "needs a local DynamoDB, see the top of this file"]
async fn shop_goes_from_website_to_refund() {
    let (paypal, llm) = (MockServer::start().await, MockServer::start().await);
    mock_paypal(&paypal).await;
    let base = start(&paypal, &llm).await;

    Mock::given(path("/chat/completions")).and(body_string_contains("response_format"))
        .respond_with(completion(json!({ "role": "assistant", "content": catalog(&base).to_string() })))
        .mount(&llm).await;
    // The hosted chat: a rate limit, then a tool call, then the final answer.
    let chat = || Mock::given(path("/chat/completions")).and(body_string_contains("\"tools\""));
    chat().respond_with(ResponseTemplate::new(429).set_body_json(json!({ "error": { "message": "slow down", "code": 429 } }))).up_to_n_times(1).with_priority(1).mount(&llm).await;
    chat().respond_with(completion(json!({ "role": "assistant", "content": null, "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "search_products", "arguments": "{\"query\":\"cake\"}" } }] }))).up_to_n_times(1).with_priority(2).mount(&llm).await;
    chat().respond_with(completion(json!({ "role": "assistant", "content": "We have a chocolate cake." }))).with_priority(3).mount(&llm).await;

    let http = Client::new();
    let owner = import_sample(&http, &base).await;
    let mcp = McpClient { http: http.clone(), url: format!("{base}/mcp/{}", owner.slug) };

    let sent = llm.received_requests().await.unwrap();
    let prompt = serde_json::to_string(&bodies(&sent, "/chat/completions")[0]).unwrap();
    assert!(prompt.contains("Alder Street"), "the crawler should read the site");
    assert!(!prompt.contains("$0.01"), "hidden text must never reach the model");

    // Nothing is reachable before the owner opens the shop.
    assert_eq!(mcp.rpc("tools/list", json!({})).await.status(), 404);
    let (status, _) = Owner { key: "wrong".into(), http: http.clone(), base: base.clone(), slug: owner.slug.clone() }.send(reqwest::Method::GET, "", None).await;
    assert_eq!(status, 403);

    let shop = owner.get("").await;
    assert_eq!(shop["verified"], json!(true), "sample shops are hosted here, so ownership is known");
    let products = shop["products"].as_array().unwrap();
    let flagged = products.iter().find(|product| product["priceVerified"] == json!(false)).unwrap();
    assert_eq!(flagged["name"], "Wedding Tasting Box");
    let cake = products.iter().find(|product| product["priceVerified"] == json!(true)).unwrap()["id"].as_str().unwrap().to_owned();

    let (status, body) = owner.publish().await;
    assert_eq!((status, body["error"].as_str().unwrap()), (400, "Confirm 1 flagged price before opening."));
    let confirm = json!({ "name": flagged["name"], "description": "", "price": 45.0, "category": "Cakes", "inStock": true });
    assert_eq!(owner.send(reqwest::Method::PUT, &format!("/products/{}", flagged["id"].as_str().unwrap()), Some(confirm)).await.0, 200);
    let (status, body) = owner.publish().await;
    assert_eq!((status, body["error"].as_str().unwrap()), (400, "Connect a PayPal account before opening."));
    assert_eq!(owner.send(reqwest::Method::PATCH, "", Some(json!({ "paypalEmail": SELLER }))).await.0, 200);
    assert_eq!(owner.publish().await.0, 200);

    // An assistant shops through MCP, and cannot choose its own price.
    let tools: Value = mcp.rpc("tools/list", json!({})).await.json().await.unwrap();
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 6);
    let (is_error, found) = mcp.call("search_products", json!({ "query": "birthday cakes", "max_price": 40 })).await;
    assert!(!is_error);
    assert_eq!(found["total_found"], 1);
    let (is_error, order) = mcp.call("create_checkout", json!({ "items": [{ "product_id": cake, "quantity": 1, "price": 0.01 }], "note": "Happy birthday" })).await;
    assert!(!is_error, "{order}");
    assert_eq!(order["total"], "34.00");
    assert_eq!(order["approval_url"], "https://paypal.test/approve");
    let reference = order["order_reference"].as_str().unwrap().to_owned();
    let (is_error, message) = mcp.call("get_product", json!({ "product_id": "nope" })).await;
    assert!(is_error, "{message}");

    let created = bodies(&paypal.received_requests().await.unwrap(), "/v2/checkout/orders").remove(0);
    let unit = &created["purchase_units"][0];
    assert_eq!(unit["payee"]["email_address"], SELLER, "the money must go to the shop");
    assert_eq!(unit["amount"]["value"], "34.00");
    assert_eq!(unit["custom_id"], format!("{}|{reference}", owner.slug));

    // A forged webhook changes nothing.
    let webhook = |signature: &'static str, event: Value| {
        http.post(format!("{base}/api/webhooks/paypal"))
            .header("paypal-auth-algo", "SHA256withRSA").header("paypal-cert-url", "https://paypal.test/cert")
            .header("paypal-transmission-id", "1").header("paypal-transmission-sig", signature).header("paypal-transmission-time", "now")
            .json(&event).send()
    };
    let approved = json!({ "event_type": "CHECKOUT.ORDER.APPROVED", "resource": { "id": "PP1", "purchase_units": [{ "custom_id": format!("{}|{reference}", owner.slug) }] } });
    assert_eq!(webhook("forged", approved.clone()).await.unwrap().status(), 400);
    assert_eq!(owner.get("/dashboard").await["stats"]["orders"], 0);

    // The buyer approves, and the assistant and PayPal's webhook both report it. One payment is recorded.
    let (_, status) = mcp.call("get_order_status", json!({ "order_reference": reference })).await;
    assert_eq!(status["status"], "paid");
    assert_eq!(webhook("good", approved).await.unwrap().status(), 200);
    let dashboard = owner.get("/dashboard").await;
    assert_eq!((dashboard["stats"]["orders"].clone(), dashboard["stats"]["revenue"].clone()), (json!(1), json!("34.00")));
    assert_eq!(dashboard["events"].as_array().unwrap().iter().filter(|event| event["kind"] == "paid").count(), 1);
    assert_eq!(dashboard["orders"][0]["buyerEmail"], "buyer@example.com");
    assert_eq!(dashboard["orders"][0]["note"], "Happy birthday");

    // The hosted chat retries the rate limit and runs its tool loop.
    let reply: Value = http.post(format!("{base}/api/shops/{}/chat", owner.slug))
        .json(&json!({ "messages": [{ "role": "user", "text": "Any cake?" }] }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(reply["reply"], "We have a chocolate cake.");
    assert_eq!(reply["steps"][0], json!({ "tool": "search_products", "title": "Search products", "ok": true }));
    let sent = llm.received_requests().await.unwrap();
    let last = bodies(&sent, "/chat/completions").pop().unwrap();
    let tool_result = last["messages"].as_array().unwrap().iter().find(|message| message["role"] == "tool").unwrap();
    assert_eq!(tool_result["tool_call_id"], "c1");

    // Tracking stays in Walk-In and reaches the shopper's assistant.
    let track = json!({ "carrier": "USPS", "number": "9400" });
    assert_eq!(owner.send(reqwest::Method::POST, &format!("/orders/{reference}/track"), Some(track)).await.0, 200);
    let (_, status) = mcp.call("get_order_status", json!({ "order_reference": reference })).await;
    assert_eq!(status["tracking"]["number"], "9400");

    // The owner refunds in their own PayPal, and the webhook brings the result back.
    assert!(owner.get("/dashboard").await["orders"][0]["paypalUrl"].as_str().unwrap().ends_with("/unifiedtransactions/details/payment/CAP1"));
    let refund = |total_refunded: &str| json!({
        "event_type": "PAYMENT.CAPTURE.REFUNDED",
        "resource": { "custom_id": format!("{}|{reference}", owner.slug), "seller_payable_breakdown": { "total_refunded_amount": { "currency_code": "USD", "value": total_refunded } } },
    });
    assert_eq!(webhook("good", refund("10.00")).await.unwrap().status(), 200);
    let dashboard = owner.get("/dashboard").await;
    assert_eq!((dashboard["orders"][0]["status"].as_str().unwrap(), dashboard["stats"]["revenue"].as_str().unwrap()), ("paid", "34.00"), "part of the money was kept");

    assert_eq!(webhook("good", refund("34.00")).await.unwrap().status(), 200);
    let dashboard = owner.get("/dashboard").await;
    assert_eq!((dashboard["orders"][0]["status"].as_str().unwrap(), dashboard["stats"]["revenue"].as_str().unwrap()), ("refunded", "0.00"));
}

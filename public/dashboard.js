import { $, api, copyButton, el, ownerKey, setSign, showFatal, slugFromPath } from "./common.js";

const slug = slugFromPath();
const key = ownerKey(slug);
const root = $("#app");
const CHANNELS = { mcp: "AI assistant", chat: "Shop chat", owner: "You" };
const STATUS = { awaiting_quote: "Needs your price", quoted: "Price sent", awaiting_payment: "Waiting for approval", paid: "Paid", refunded: "Refunded", cancelled: "Cancelled", declined: "Declined", failed: "Failed" };

let built = false;
let previous = {};
const slots = {};

const time = (iso) => new Date(iso).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });

function connectionRow(label, value) {
  return el("div", { class: "connect-row" }, el("span", { class: "connect-label" }, label), el("code", { class: "code-line" }, value), copyButton(() => value));
}

function build(data) {
  slots.visits = el("strong", {});
  slots.orders = el("strong", {});
  slots.revenue = el("strong", {});
  slots.feed = el("ol", { class: "feed", "aria-live": "polite" });
  slots.orderList = el("div", { class: "orders" });
  slots.unmet = el("ul", { class: "unmet" });
  const { connection, shop } = data;
  const privateLink = `${location.origin}/dashboard/${slug}#key=${encodeURIComponent(key)}`;

  root.replaceChildren(
    el("div", { class: "dash-head" }, el("div", {}, el("p", { class: "eyebrow" }, "Open for business"), el("h1", {}, shop.name))),
    el("section", { class: "stats" },
      el("div", { class: "stat" }, slots.visits, el("span", {}, "AI visits")),
      el("div", { class: "stat" }, slots.orders, el("span", {}, "paid orders")),
      el("div", { class: "stat" }, slots.revenue, el("span", {}, `revenue, ${shop.currency}`)),
    ),
    el("section", { class: "card" },
      el("h2", {}, "Your shop's door"),
      el("p", { class: "muted" }, "Give this address to any assistant that supports MCP. It can search your shop and start a PayPal checkout."),
      connectionRow("Assistant address", connection.mcpUrl),
      connectionRow("Claude Code", connection.claudeCode),
      el("div", { class: "row" },
        el("a", { class: "button button-primary button-small", href: connection.shopperUrl, target: "_blank", rel: "noopener" }, "Try it as a shopper"),
        copyButton(() => privateLink, "Copy my private owner link"),
      ),
    ),
    el("div", { class: "dash-grid" },
      el("section", { class: "card" }, el("h2", {}, "Orders and quote requests"), slots.orderList),
      el("div", { class: "dash-side" },
        el("section", { class: "card card-accent" }, el("h2", {}, "What people wanted that you don't sell"), slots.unmet),
        el("section", { class: "card" }, el("h2", {}, "Right now in your shop"), slots.feed),
      ),
    ),
  );
  setSign("open");
  built = true;
}

function renderFeed(events) {
  slots.feed.replaceChildren(...(events.length ? events.map((event) => el("li", { class: `feed-item feed-${event.kind}` },
    el("span", { class: "feed-time" }, time(event.createdAt)),
    el("span", {}, event.summary),
    el("span", { class: "chip chip-static" }, CHANNELS[event.channel] ?? event.channel),
  )) : [el("li", { class: "muted" }, "Nobody has walked in yet.")]));
}

function renderUnmet(unmet) {
  slots.unmet.replaceChildren(...(unmet.length ? unmet.map((entry) => el("li", {}, el("span", {}, entry.need), el("span", { class: "chip chip-static" }, `${entry.count} ${entry.count === 1 ? "ask" : "asks"}`))) : [el("li", { class: "muted" }, "Nothing yet. When an assistant can't find something, it shows up here.")]));
}

async function act(path, body, message) {
  message.textContent = "Working...";
  document.activeElement?.blur();
  try {
    await api(`/api/shops/${slug}/orders/${path}`, { method: "POST", key, body });
    previous.orders = null;
    await refresh();
  } catch (failure) {
    message.textContent = failure.message;
  }
}

function quoteForm(order, message) {
  const price = el("input", { type: "number", min: "0", step: "any", required: true, placeholder: `Price in ${order.currency}`, "aria-label": `Full price in ${order.currency}` });
  const note = el("input", { placeholder: "Note (optional)", maxlength: "500", "aria-label": "Message to the shopper" });
  if (order.status === "quoted") {
    price.value = order.total;
    note.value = order.quoteNote ?? "";
  }
  return el("form", {
    class: "track-form",
    onsubmit: (event) => {
      event.preventDefault();
      act(`${order.id}/quote`, { price: Number(price.value), note: note.value }, message);
    },
  }, price, note,
    el("button", { class: "button button-primary button-small", type: "submit" }, order.status === "quoted" ? "Update price" : "Send price"),
    el("button", { class: "button button-text button-small", type: "button", onclick: () => act(`${order.id}/decline`, { note: note.value }, message) }, "Decline"),
  );
}

function orderCard(order) {
  const message = el("p", { class: "muted", role: "status" });
  const open = order.status === "awaiting_quote" || order.status === "quoted";
  const priced = order.status !== "awaiting_quote" && order.status !== "declined";
  const carrier = el("input", { placeholder: "Carrier", "aria-label": "Carrier" });
  const number = el("input", { placeholder: "Tracking number", "aria-label": "Tracking number" });
  const paid = order.status === "paid";
  return el("article", { class: "order" },
    el("div", { class: "order-head" },
      el("strong", {}, order.id),
      el("span", { class: `chip chip-static status-${order.status}` }, STATUS[order.status] ?? order.status),
      priced ? el("span", { class: "order-total" }, `${order.total} ${order.currency}`) : null,
    ),
    order.request
      ? el("p", { class: "order-note" }, order.request)
      : el("ul", { class: "order-items" }, order.items.map((item) => el("li", {}, `${item.quantity} × ${item.name}`, item.options.length ? ` (${item.options.map(([, value]) => value).join(", ")})` : "", el("span", {}, item.unit)))),
    el("p", { class: "muted" }, [
      order.buyerName || order.buyerEmail ? `${order.buyerName ?? ""} ${order.buyerEmail ? `<${order.buyerEmail}>` : ""}`.trim() : null,
      order.fulfillment === "ship" ? order.shipTo ? `Ship to ${order.shipTo.name}, ${order.shipTo.address}` : "To be shipped" : "Pickup",
      order.contact ? `Contact: ${order.contact}` : null,
      `via ${CHANNELS[order.channel] ?? order.channel}`,
    ].filter(Boolean).join(" · ")),
    order.note ? el("p", { class: "order-note" }, `Note: ${order.note}`) : null,
    order.quoteNote && !open ? el("p", { class: "muted" }, `You told them: ${order.quoteNote}`) : null,
    open ? quoteForm(order, message) : null,
    open ? el("div", { class: "row" },
      el("p", { class: "muted" }, "Include any delivery in your price. The shopper pays it through PayPal from this link:"),
      copyButton(() => `${location.origin}/quote/${slug}/${order.id}`, "Copy shopper link"),
    ) : null,
    order.tracking ? el("p", { class: "muted" }, `Tracking: ${order.tracking.carrier} ${order.tracking.number}`) : null,
    paid ? el("div", { class: "row" },
      order.fulfillment === "ship" && !order.tracking ? el("form", {
        class: "track-form",
        onsubmit: (event) => {
          event.preventDefault();
          act(`${order.id}/track`, { carrier: carrier.value, number: number.value }, message);
        },
      }, carrier, number, el("button", { class: "button button-quiet button-small", type: "submit" }, "Add tracking")) : null,
      order.paypalUrl ? el("a", { class: "button button-text button-small", href: order.paypalUrl, target: "_blank", rel: "noopener" }, "Refund in PayPal") : null,
    ) : null,
    paid && order.paypalUrl ? el("p", { class: "muted" }, "Refunds happen in your own PayPal account. This page updates when PayPal confirms one.") : null,
    message,
  );
}

function update(name, value, render) {
  const serialized = JSON.stringify(value);
  if (previous[name] !== serialized) {
    previous[name] = serialized;
    render(value);
  }
}

async function refresh() {
  const data = await api(`/api/shops/${slug}/dashboard`, { key });
  if (data.shop.status !== "live") {
    location.replace(`/setup/${slug}`);
    return false;
  }
  if (!built) build(data);
  document.title = `${data.shop.name} · Walk-In`;
  slots.visits.textContent = data.stats.visits;
  slots.orders.textContent = data.stats.orders;
  slots.revenue.textContent = data.stats.revenue;
  update("events", data.events, renderFeed);
  update("unmet", data.unmet, renderUnmet);
  // Replacing the list would wipe a price the owner is typing, so it waits until they leave the field.
  if (!slots.orderList.contains(document.activeElement)) {
    const needsPriceFirst = [...data.orders].sort((a, b) => (b.status === "awaiting_quote") - (a.status === "awaiting_quote"));
    update("orders", needsPriceFirst, (orders) => slots.orderList.replaceChildren(...(orders.length ? orders.map(orderCard) : [el("p", { class: "muted" }, "Orders and price requests will appear here the moment a shopper starts one.")])));
  }
  return true;
}

async function loop() {
  try {
    if (await refresh()) setTimeout(loop, 4000);
  } catch (failure) {
    if (built) setTimeout(loop, 8000);
    else showFatal(root, "We can't open this shop", failure.message);
  }
}

if (key) loop();
else showFatal(root, "This link is missing its key", "Open the private link you were given when you set up this shop.");

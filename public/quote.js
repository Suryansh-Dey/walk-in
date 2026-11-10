import { $, api, el, safeUrl, setSign, showFatal, slugFromPath } from "./common.js";

const slug = slugFromPath();
const reference = location.pathname.split("/")[3];
const root = $("#app");
const SETTLED = new Set(["paid", "refunded", "declined", "failed", "cancelled"]);
const VIEWS = {
  awaiting_quote: ["The shop is working out a price", "You'll see it here as soon as they send it. This page updates by itself."],
  quoted: ["Your price is ready", "Nothing is charged until you approve it in PayPal."],
  awaiting_payment: ["Your price is ready", "Nothing is charged until you approve it in PayPal."],
  paid: ["Paid. Thank you.", "Your payment went directly to the shop through PayPal."],
  refunded: ["This payment was refunded", "The shop returned your money through PayPal."],
  declined: ["The shop can't take this one", "Nothing was charged."],
};

let shown;

async function pay(button, message) {
  button.disabled = true;
  message.textContent = "Opening PayPal...";
  try {
    const { approvalUrl } = await api(`/api/shops/${slug}/quotes/${reference}/pay`, { method: "POST" });
    const url = safeUrl(approvalUrl);
    if (!url) throw new Error("PayPal didn't return a link. Please try again.");
    location.href = url;
  } catch (failure) {
    message.textContent = failure.message;
    button.disabled = false;
  }
}

function render(request) {
  const [heading, detail] = VIEWS[request.status] ?? ["This request has closed", "Nothing was charged."];
  const payable = request.status === "quoted" || request.status === "awaiting_payment";
  const message = el("p", { class: "muted", role: "status" });
  const button = el("button", { type: "button", class: "button button-paypal", onclick: () => pay(button, message) }, "Pay with PayPal");
  document.title = `${request.shop} · Walk-In`;
  root.replaceChildren(el("section", { class: "notice" },
    el("p", { class: "eyebrow" }, `${request.shop} · ${request.reference}`),
    el("h1", {}, heading),
    el("p", { class: "muted" }, detail),
    el("p", { class: "order-note" }, request.request),
    request.note ? el("p", {}, `From the shop: ${request.note}`) : null,
    payable ? el("div", { class: "pay-card" }, el("div", {}, el("span", { class: "muted" }, "Total"), el("strong", {}, `${request.total} ${request.currency}`)), button) : null,
    message,
  ));
}

async function refresh() {
  let request;
  try {
    request = await api(`/api/shops/${slug}/quotes/${reference}`);
  } catch (failure) {
    if (shown) setTimeout(refresh, 15000);
    else showFatal(root, "We can't find that request", failure.message);
    return;
  }
  const serialized = JSON.stringify(request);
  if (serialized !== shown) {
    shown = serialized;
    setSign("open");
    render(request);
  }
  if (!SETTLED.has(request.status)) setTimeout(refresh, 5000);
}

refresh();

import { $, api, el, safeUrl, setSign, showFatal, slugFromPath } from "./common.js";

const slug = slugFromPath();
const root = $("#app");
const STARTERS = ["What do you sell?", "What would you suggest as a gift?", "How does delivery work?"];
const history = [];

let log;
let input;
let form;
let starters;

function scroll() {
  log.lastElementChild?.scrollIntoView({ behavior: "smooth", block: "end" });
}

function checkoutCard(checkout) {
  const url = safeUrl(checkout.approvalUrl);
  return el("div", { class: "pay-card" },
    el("div", {}, el("span", { class: "muted" }, `Order ${checkout.orderReference}`), el("strong", {}, `${checkout.total} ${checkout.currency}`)),
    url ? el("a", { class: "button button-paypal", href: url, target: "_blank", rel: "noopener" }, "Approve in PayPal") : null,
  );
}

function quoteCard(quote) {
  const url = safeUrl(quote.statusUrl);
  return el("div", { class: "pay-card" },
    el("div", {}, el("span", { class: "muted" }, `Request ${quote.orderReference}`), el("strong", {}, "Your price request")),
    url ? el("a", { class: "button button-primary", href: url, target: "_blank", rel: "noopener" }, "Open request") : null,
  );
}

function addAssistant(result) {
  const bubble = el("div", { class: "turn turn-assistant" },
    result.steps.length ? el("ul", { class: "tool-steps" }, result.steps.map((step) => el("li", { class: step.ok ? "chip chip-static" : "chip chip-static is-failed" }, step.title))) : null,
    el("p", { class: "bubble" }, result.reply),
    result.checkout ? checkoutCard(result.checkout) : null,
    result.quote ? quoteCard(result.quote) : null,
  );
  log.append(bubble);
  scroll();
}

async function send(text) {
  const message = text.trim();
  if (!message) return;
  starters.hidden = true;
  log.append(el("div", { class: "turn turn-user" }, el("p", { class: "bubble" }, message)));
  history.push({ role: "user", text: message });
  const thinking = el("div", { class: "turn turn-assistant", id: "thinking" }, el("p", { class: "bubble bubble-thinking" }, "Looking around the shop..."));
  log.append(thinking);
  scroll();
  input.value = "";
  form.querySelector("button").disabled = true;
  try {
    const result = await api(`/api/shops/${slug}/chat`, { method: "POST", body: { messages: history.slice(-30) } });
    thinking.remove();
    history.push({ role: "assistant", text: result.reply.slice(0, 2000) });
    addAssistant(result);
  } catch (failure) {
    thinking.remove();
    history.pop();
    log.append(el("div", { class: "turn turn-assistant" }, el("p", { class: "bubble is-error" }, failure.message)));
    scroll();
  } finally {
    form.querySelector("button").disabled = false;
    input.focus();
  }
}

function render(shop) {
  log = el("div", { class: "chat-log", role: "log", "aria-live": "polite" });
  input = el("input", { type: "text", "aria-label": "Your message", placeholder: "Ask for what you're looking for", maxlength: "2000", autocomplete: "off" });
  form = el("form", { class: "chat-form", onsubmit: (event) => { event.preventDefault(); send(input.value); } }, input, el("button", { class: "button button-primary", type: "submit" }, "Send"));
  starters = el("div", { class: "chips" }, STARTERS.map((text) => el("button", { type: "button", class: "chip", onclick: () => send(text) }, text)));
  const website = safeUrl(shop.website);
  root.replaceChildren(el("section", { class: "chat" },
    el("div", { class: "chat-head" },
      el("h1", {}, shop.name),
      shop.description ? el("p", { class: "muted" }, shop.description) : null,
      website ? el("a", { class: "muted-link", href: website, target: "_blank", rel: "noopener" }, shop.verified ? "Visit the shop's website (confirmed by the owner)" : "Visit the shop's website") : null,
    ),
    log,
    starters,
    form,
    el("p", { class: "fineprint" }, "An AI assistant answers here. Nothing is charged until you approve the order in PayPal."),
  ));
  input.focus();
}

api(`/api/shops/${slug}/public`)
  .then((shop) => {
    document.title = `${shop.name} · Walk-In`;
    setSign("open");
    render(shop);
  })
  .catch((failure) => showFatal(root, "This shop isn't open", failure.message));

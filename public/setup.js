import { $, api, copyButton, el, ownerKey, setSign, showFatal, slugFromPath } from "./common.js";

const slug = slugFromPath();
const key = ownerKey(slug);
const root = $("#app");
let shop;
let status;
let checklist;

const flagged = () => shop.products.filter((product) => !product.priceVerified).length;

function setStatus(message, isError = false) {
  if (!status) return;
  status.textContent = message;
  status.classList.toggle("is-error", isError);
}

async function save(patch, { rerender = false } = {}) {
  setStatus("Saving...");
  try {
    shop = await api(`/api/shops/${slug}`, { method: "PATCH", body: patch, key });
    setStatus("Saved");
    if (rerender) renderReview();
    else renderChecklist();
  } catch (failure) {
    setStatus(failure.message, true);
  }
}

function field(label, name, { multiline = false, type = "text", placeholder = "" } = {}) {
  const id = `field-${name}`;
  const input = el(multiline ? "textarea" : "input", { id, placeholder, rows: multiline ? 3 : null, type: multiline ? null : type });
  input.value = shop[name] ?? "";
  input.addEventListener("change", () => save({ [name]: input.value }));
  return el("div", { class: "field" }, el("label", { for: id }, label), input);
}

function renderProgress() {
  const steps = shop.progress.length ? shop.progress : [{ label: "Getting ready", done: false }];
  root.replaceChildren(el("section", { class: "notice" },
    el("p", { class: "eyebrow" }, "One moment"),
    el("h1", {}, "Walking through your shop"),
    el("ol", { class: "progress" }, steps.map((step) => el("li", { class: step.done ? "is-done" : "is-active" }, step.label))),
    el("p", { class: "muted" }, "This usually takes under a minute."),
  ));
}

function productRow(product) {
  const needsConfirming = !product.priceVerified;
  const name = el("input", { "aria-label": "Product name" });
  name.value = product.name;
  const price = el("input", { type: "number", min: "0", step: "any", "aria-label": `Price in ${shop.currency}` });
  price.value = product.price;
  const category = el("input", { "aria-label": "Category", placeholder: "Category" });
  category.value = product.category;
  const inStock = el("input", { type: "checkbox" });
  inStock.checked = product.inStock;

  const row = el("li", { class: needsConfirming ? "product is-flagged" : "product" });
  const submit = async () => {
    setStatus("Saving...");
    try {
      const saved = await api(`/api/shops/${slug}/products/${product.id}`, {
        method: "PUT",
        key,
        body: { name: name.value, description: product.description, price: Number(price.value), category: category.value, inStock: inStock.checked },
      });
      shop.products[shop.products.findIndex((entry) => entry.id === product.id)] = saved;
      row.replaceWith(productRow(saved));
      setStatus("Saved");
      renderChecklist();
    } catch (failure) {
      setStatus(failure.message, true);
    }
  };
  const remove = async () => {
    try {
      await api(`/api/shops/${slug}/products/${product.id}`, { method: "DELETE", key });
      shop.products = shop.products.filter((entry) => entry.id !== product.id);
      row.remove();
      renderChecklist();
    } catch (failure) {
      setStatus(failure.message, true);
    }
  };

  row.append(...[
    el("div", { class: "product-fields" }, name, el("div", { class: "price-box" }, el("span", {}, shop.currency), price), category),
    needsConfirming ? el("p", { class: "flag" }, product.priceText ? `We read this as "${product.priceText}" and couldn't match it to a number on your page. Please check it.` : "We couldn't find this price on your page. Please check it.") : null,
    el("div", { class: "product-actions" },
      el("label", { class: "check" }, inStock, "In stock"),
      el("button", { type: "button", class: needsConfirming ? "button button-primary button-small" : "button button-quiet button-small", onclick: submit }, needsConfirming ? "Confirm price" : "Save"),
      el("button", { type: "button", class: "button button-text button-small", onclick: remove }, "Remove"),
    ),
  ].filter(Boolean));
  return row;
}

function addProductForm(list) {
  const name = el("input", { placeholder: "Product name", "aria-label": "New product name" });
  const price = el("input", { type: "number", min: "0", step: "any", placeholder: "Price", "aria-label": "New product price" });
  const form = el("form", {
    class: "add-product",
    onsubmit: async (event) => {
      event.preventDefault();
      try {
        const created = await api(`/api/shops/${slug}/products`, { method: "POST", key, body: { name: name.value, price: Number(price.value) } });
        shop.products.push(created);
        list.append(productRow(created));
        form.reset();
        renderChecklist();
      } catch (failure) {
        setStatus(failure.message, true);
      }
    },
  }, name, price, el("button", { class: "button button-quiet button-small", type: "submit" }, "Add product"));
  return form;
}

function fulfillmentSection() {
  const toggle = (label, name) => {
    const input = el("input", { type: "checkbox" });
    input.checked = shop[name];
    input.addEventListener("change", () => save({ [name]: input.checked }));
    return el("label", { class: "check" }, input, label);
  };
  const fee = el("input", { id: "field-shipping", type: "number", min: "0", step: "any" });
  fee.value = shop.shipping;
  fee.addEventListener("change", () => save({ shipping: Number(fee.value) }));
  return el("div", {},
    el("div", { class: "check-row" }, toggle("Ships orders", "ships"), toggle("Customers can pick up", "pickup")),
    el("div", { class: "field" }, el("label", { for: "field-shipping" }, `Flat shipping fee (${shop.currency})`), fee),
  );
}

function ownershipBlock() {
  const tag = `<meta name="walk-in-verify" content="${shop.verifyToken}">`;
  const result = el("p", { class: "muted", role: "status" });
  return el("div", { class: "side-block" },
    el("h3", {}, "Prove this is your site"),
    el("p", { class: "muted" }, "Add this tag inside <head> on your home page, then check it."),
    el("code", { class: "code-line" }, tag),
    el("div", { class: "row" }, copyButton(() => tag), el("button", {
      type: "button",
      class: "button button-quiet button-small",
      onclick: async () => {
        result.textContent = "Checking...";
        try {
          const { verified } = await api(`/api/shops/${slug}/verify`, { method: "POST", key });
          shop.verified = verified;
          result.textContent = verified ? "Confirmed." : "We couldn't find the tag yet.";
          renderChecklist();
        } catch (failure) {
          result.textContent = failure.message;
        }
      },
    }, "Check my site")),
    result,
  );
}

function paypalBlock() {
  const email = el("input", { id: "field-paypal", type: "email", placeholder: "you@yourshop.com", autocomplete: "email" });
  email.value = shop.paypalEmail;
  email.addEventListener("change", () => save({ paypalEmail: email.value }));
  return el("div", { class: "side-block" },
    el("h3", {}, "Where should the money go?"),
    el("div", { class: "field" }, el("label", { for: "field-paypal" }, "Your PayPal business email"), email),
    shop.demoMerchantEmail ? el("button", {
      type: "button",
      class: "button button-text button-small",
      onclick: () => {
        email.value = shop.demoMerchantEmail;
        save({ paypalEmail: email.value });
      },
    }, "Use the sandbox demo seller") : null,
    shop.paypalReady ? null : el("p", { class: "flag" }, "This server has no PayPal credentials yet, so checkout won't work."),
  );
}

function renderChecklist() {
  if (!checklist) return;
  const count = flagged();
  const needsOwnership = shop.publishNeedsVerification && !shop.verified;
  const items = [
    [shop.products.length > 0, `${shop.products.length} ${shop.products.length === 1 ? "product" : "products"} found`],
    [count === 0, count === 0 ? "Every price checked" : `${count} ${count === 1 ? "price needs" : "prices need"} your eye`],
    [Boolean(shop.paypalEmail), shop.paypalEmail ? "PayPal connected" : "Connect PayPal"],
    [shop.ships || shop.pickup, shop.ships || shop.pickup ? "Delivery set" : "Offer shipping or pickup"],
    needsOwnership || shop.publishNeedsVerification ? [shop.verified, shop.verified ? "Website confirmed as yours" : "Prove the website is yours"] : null,
  ].filter(Boolean);
  const publish = el("button", { type: "button", class: "button button-primary button-wide", disabled: !items.every(([done]) => done), onclick: open }, "Open the shop");
  checklist.replaceChildren(
    el("h3", {}, "Before you open"),
    el("ul", { class: "progress" }, items.map(([done, label]) => el("li", { class: done ? "is-done" : "is-todo" }, label))),
    publish,
  );
}

async function open(event) {
  event.target.disabled = true;
  try {
    await api(`/api/shops/${slug}/publish`, { method: "POST", key });
    setSign("open");
    setTimeout(() => (location.href = `/dashboard/${slug}`), 1100);
  } catch (failure) {
    setStatus(failure.message, true);
    event.target.disabled = false;
  }
}

function renderReview() {
  status = el("p", { class: "save-status", role: "status" });
  checklist = el("div", { class: "side-block" });
  const flaggedFirst = [...shop.products].sort((a, b) => a.priceVerified - b.priceVerified);
  const list = el("ul", { class: "products" }, flaggedFirst.map(productRow));
  const currency = el("input", { id: "field-currency", maxlength: "3" });
  currency.value = shop.currency;
  currency.addEventListener("change", () => save({ currency: currency.value.trim().toUpperCase() }, { rerender: true }));

  root.replaceChildren(el("div", { class: "review" },
    el("div", { class: "review-main" },
      el("p", { class: "eyebrow" }, "Check your shop"),
      el("h1", {}, shop.name || "Your shop"),
      shop.warnings.length ? el("ul", { class: "warnings" }, shop.warnings.map((warning) => el("li", {}, warning))) : null,
      el("section", { class: "card" },
        el("h2", {}, "What you sell"),
        el("p", { class: "muted" }, "Assistants can only quote these prices, so make sure they're right."),
        list,
        addProductForm(list),
      ),
      el("section", { class: "card" },
        el("h2", {}, "About the shop"),
        field("Shop name", "name"),
        field("Description", "description", { multiline: true }),
        el("div", { class: "field-pair" }, field("Location", "location"), field("Contact", "contact")),
        el("div", { class: "field-pair" }, field("Hours", "hours"), el("div", { class: "field" }, el("label", { for: "field-currency" }, "Currency"), currency)),
      ),
      el("section", { class: "card" },
        el("h2", {}, "Delivery and returns"),
        fulfillmentSection(),
        field("Shipping policy", "shippingPolicy", { multiline: true }),
        field("Returns policy", "returnsPolicy", { multiline: true }),
      ),
    ),
    el("aside", { class: "review-side" },
      paypalBlock(),
      shop.publishNeedsVerification && !shop.verified ? ownershipBlock() : null,
      checklist,
      status,
    ),
  ));
  renderChecklist();
}

async function poll() {
  try {
    shop = await api(`/api/shops/${slug}`, { key });
  } catch (failure) {
    showFatal(root, "We can't open this shop", failure.message);
    return;
  }
  document.title = `${shop.name || "Your shop"} · Walk-In`;
  if (shop.status === "live") {
    location.replace(`/dashboard/${slug}`);
  } else if (shop.status === "importing") {
    renderProgress();
    setTimeout(poll, 1200);
  } else if (shop.status === "failed") {
    showFatal(root, "We couldn't read that website", shop.error || "Please try again.");
  } else {
    renderReview();
  }
}

if (key) poll();
else showFatal(root, "This link is missing its key", "Open the private link you were given when you set up this shop.");

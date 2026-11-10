// Everything shown from a scraped site goes through text nodes or checked URLs, never as HTML.

export const $ = (selector, root = document) => root.querySelector(selector);

export function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (name === "class") node.className = value;
    else if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
    else node.setAttribute(name, value === true ? "" : value);
  }
  node.append(...children.flat().filter((child) => child !== null && child !== undefined && child !== false));
  return node;
}

export async function api(path, { method = "GET", body, key } = {}) {
  const headers = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  if (key) headers["x-owner-key"] = key;
  const response = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || "Something went wrong. Please try again.");
  return data;
}

export const slugFromPath = () => location.pathname.split("/")[2];

const memory = new Map();

/** Reads the owner key from a private link or this browser. There are no accounts to recover it from. */
export function ownerKey(slug) {
  const storageKey = `walk-in:${slug}`;
  const fromLink = new URLSearchParams(location.hash.slice(1)).get("key");
  if (fromLink) {
    rememberKey(slug, fromLink);
    history.replaceState(null, "", location.pathname);
    return fromLink;
  }
  if (memory.has(storageKey)) return memory.get(storageKey);
  try {
    return localStorage.getItem(storageKey);
  } catch {
    return null;
  }
}

export function rememberKey(slug, key) {
  const storageKey = `walk-in:${slug}`;
  memory.set(storageKey, key);
  try {
    localStorage.setItem(storageKey, key);
  } catch {
    // Private windows can refuse storage. The key stays in memory and in the link.
  }
}

export function safeUrl(url) {
  try {
    const parsed = new URL(url, location.href);
    return parsed.protocol === "https:" || parsed.protocol === "http:" ? parsed.href : null;
  } catch {
    return null;
  }
}

export function setSign(state) {
  const sign = $("#sign");
  if (!sign) return;
  sign.dataset.state = state;
  sign.setAttribute("aria-label", `Shop sign: ${state}`);
}

export function copyButton(getText, label = "Copy") {
  const button = el("button", {
    type: "button",
    class: "button button-quiet button-small",
    onclick: async () => {
      try {
        await navigator.clipboard.writeText(getText());
        button.textContent = "Copied";
      } catch {
        button.textContent = "Press Ctrl+C";
      }
      setTimeout(() => (button.textContent = label), 1600);
    },
  }, label);
  return button;
}

export function showFatal(root, title, message) {
  root.replaceChildren(el("section", { class: "notice" }, el("h1", {}, title), el("p", { class: "muted" }, message), el("a", { class: "button button-primary", href: "/" }, "Back to the start")));
}

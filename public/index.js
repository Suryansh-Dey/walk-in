import { $, api, el, rememberKey, setSign } from "./common.js";

const error = $("#error");

function showError(message) {
  error.textContent = message;
  error.hidden = !message;
}

async function start(url, trigger) {
  showError("");
  trigger.disabled = true;
  try {
    const { slug, key } = await api("/api/shops", { method: "POST", body: { url } });
    rememberKey(slug, key);
    location.href = `/setup/${slug}#key=${encodeURIComponent(key)}`;
  } catch (failure) {
    showError(failure.message);
    trigger.disabled = false;
  }
}

$("#start").addEventListener("submit", (event) => {
  event.preventDefault();
  const url = $("#url").value.trim();
  if (!url) {
    showError("Enter your website address.");
    return;
  }
  start(url, event.target.querySelector("button"));
});

api("/api/samples")
  .then(({ samples }) => {
    if (!samples.length) return;
    $("#samples").append(...samples.map((sample) => {
      const chip = el("button", { type: "button", class: "chip", title: sample.blurb, onclick: () => start(sample.url, chip) }, sample.name);
      return chip;
    }));
    $("#samples-wrap").hidden = false;
  })
  .catch(() => {});

setTimeout(() => setSign("open"), 900);

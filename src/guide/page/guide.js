// SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
// SPDX-License-Identifier: Apache-2.0 OR MIT

"use strict";

const $ = (id) => document.getElementById(id);
let polling = false;

function kind() {
  return document.querySelector("input[name=kind]:checked").value;
}

function showKind() {
  const k = kind();
  for (const el of document.querySelectorAll("[data-kind]")) {
    el.hidden = !el.dataset.kind.split(" ").includes(k);
  }
}

function lines(text) {
  return text.split("\n").map((l) => l.trim()).filter((l) => l);
}

function form() {
  const k = kind();
  return {
    source: {
      kind: k,
      url: $("src-url").value.trim(),
      login: k === "imap" ? $("src-imap-login").value.trim() : $("src-jmap-login").value.trim(),
      path: $("src-path").value.trim(),
      cleartext: $("src-cleartext").checked,
      secret: $("src-secret").value,
    },
    accounts: lines($("accounts").value),
    create: $("create").checked,
    target: { url: $("dst-url").value.trim(), token: $("dst-token").value },
    parallel: Number($("parallel").value) || 4,
  };
}

async function call(path, body) {
  const init = body === undefined ? {} : {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  };
  const r = await fetch(path, init);
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || `The guide answered ${r.status}.`);
  return data;
}

function status(id, ok, message) {
  const el = $(id);
  el.className = "status " + (ok ? "ok" : "bad");
  el.textContent = message;
}

// The list as the server reads it: tidied in place, with what it couldn't use.
async function checkAccounts() {
  const r = await call("api/accounts", { text: $("accounts").value });
  $("accounts").value = r.accounts.join("\n");
  let message = r.accounts.length === 1 ? "1 account." : `${r.accounts.length} accounts.`;
  if (r.rejected.length) message += ` Left out, no address: ${r.rejected.join("; ")}`;
  status("accounts-status", r.accounts.length > 0 && !r.rejected.length, message);
  return r.accounts;
}

async function check(path, statusId) {
  try {
    if (statusId === "source-status" && kind() === "maildir") await checkAccounts();
    const r = await call(path, { form: form() });
    status(statusId, r.ok, r.message);
    await refresh();
  } catch (e) {
    status(statusId, false, e.message);
  }
}

async function start(jobKind) {
  const box = jobKind === "directory" ? "accounts-status" : jobKind === "preview" ? "preview-job" : "run-job";
  try {
    if (jobKind !== "directory") await checkAccounts();
    await call("api/job", { kind: jobKind, form: form() });
    await refresh();
  } catch (e) {
    if (box === "accounts-status") status(box, false, e.message);
    else { $(box).replaceChildren(); $(box).append(para("summary bad", e.message)); }
  }
}

function para(cls, text) {
  const p = document.createElement("p");
  p.className = cls;
  p.textContent = text;
  return p;
}

function parseCsv(text) {
  const rows = [];
  let row = [], field = "", quoted = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (quoted) {
      if (c === '"' && text[i + 1] === '"') { field += '"'; i++; }
      else if (c === '"') quoted = false;
      else field += c;
    } else if (c === '"') quoted = true;
    else if (c === ",") { row.push(field); field = ""; }
    else if (c === "\n") { row.push(field); rows.push(row); row = []; field = ""; }
    else if (c !== "\r") field += c;
  }
  if (field || row.length) { row.push(field); rows.push(row); }
  return rows;
}

function reportTable(csv) {
  const [head, ...rows] = parseCsv(csv);
  const show = ["account", "step", "result", "fetched", "created", "updated", "skipped", "failed", "detail"];
  const at = show.map((name) => head.indexOf(name));
  const table = document.createElement("table");
  const tr = table.insertRow();
  for (const name of show) {
    const th = document.createElement("th");
    th.textContent = name;
    tr.append(th);
  }
  for (const r of rows) {
    const row = table.insertRow();
    const result = r[head.indexOf("result")];
    if (result === "failed") row.className = "bad";
    else if (result === "partial" || result === "not run") row.className = "warn";
    at.forEach((i, n) => {
      const td = row.insertCell();
      td.textContent = r[i] ?? "";
      if (n >= 3 && n <= 7) td.className = "num";
    });
  }
  return table;
}

function summary(job) {
  const progress = job.lines.map((l) => l.match(/^\[(\d+)\/(\d+)\]/)).filter((m) => m).pop();
  if (job.running) {
    return progress ? `Running: ${progress[1]} of ${progress[2]} accounts done.` : "Running…";
  }
  const preview = job.kind === "preview";
  const report = (job.result && job.result.report) || "";
  if (/^\(setup\),[^,]*,failed,/m.test(report)) {
    return (preview ? "Setting up the accounts would fail" : "Setting up the accounts failed") +
      ", so no account was " + (preview ? "previewed" : "moved") + ". The table says why.";
  }
  if (job.exit === 0) return preview ? "Nothing in the preview failed." : "Every account moved.";
  if (job.exit === 5) {
    return preview
      ? "Some accounts would fail. The table says why."
      : "Some accounts didn't finish. Fix what the table says and start again: it picks up where each stopped.";
  }
  return (preview ? "The preview" : "The run") + " stopped. The output says why.";
}

function renderJob(job) {
  if (job.kind === "directory") {
    if (job.running) status("accounts-status", true, "Reading the list from the server…");
    else if (job.result && job.result.accounts) {
      $("accounts").value = job.result.accounts.join("\n");
      status("accounts-status", true, `Read ${job.result.accounts.length} accounts. Remove any that shouldn't move.`);
    } else status("accounts-status", false, "The list couldn't be read: " + (job.lines.slice(-1)[0] || ""));
    return;
  }
  const box = $(job.kind === "preview" ? "preview-job" : "run-job");
  const pre = document.createElement("pre");
  pre.textContent = job.lines.slice(-40).join("\n");
  const ok = job.running || job.exit === 0;
  box.replaceChildren(para("summary " + (ok ? "" : "bad"), summary(job)), pre);
  const result = job.result || {};
  if (result.report) box.append(reportTable(result.report), para("note", "Report: " + result.reportPath));
  if (result.passwords) box.append(para("note", "The new accounts' passwords are in " + result.passwords));
  pre.scrollTop = pre.scrollHeight;
}

async function refresh() {
  const s = await call("api/state");
  $("dir").textContent = s.dir;
  $("command").textContent = s.command;
  $("src-secret-kept").hidden = !s.hasSourceSecret;
  $("dst-token-kept").hidden = !s.hasTargetToken;
  const running = s.job && s.job.running;
  for (const id of ["preview", "run", "final", "read-directory"]) $(id).disabled = running;
  if (s.job) renderJob(s.job);
  if (running && !polling) {
    polling = true;
    setTimeout(() => { polling = false; refresh(); }, 1000);
  }
  return s;
}

async function load() {
  const s = await refresh();
  const f = s.form;
  if (f.source) {
    document.querySelector(`input[name=kind][value=${f.source.kind}]`).checked = true;
    $("src-url").value = f.source.url || "";
    if (f.source.kind === "imap" && f.source.login) $("src-imap-login").value = f.source.login;
    if (f.source.kind === "jmap") $("src-jmap-login").value = f.source.login || "";
    if (f.source.path) $("src-path").value = f.source.path;
    $("src-cleartext").checked = !!f.source.cleartext;
    $("accounts").value = (f.accounts || []).join("\n");
    $("create").checked = f.create || !f.target.url;
    $("dst-url").value = f.target.url || "";
    if (f.parallel) $("parallel").value = f.parallel;
  }
  showKind();
}

document.addEventListener("DOMContentLoaded", () => {
  for (const r of document.querySelectorAll("input[name=kind]")) r.addEventListener("change", showKind);
  $("check-source").addEventListener("click", () => check("api/check-source", "source-status"));
  $("check-target").addEventListener("click", () => check("api/check-target", "target-status"));
  $("read-accounts").addEventListener("click", () => checkAccounts().catch((e) => status("accounts-status", false, e.message)));
  $("read-directory").addEventListener("click", () => start("directory"));
  $("preview").addEventListener("click", () => start("preview"));
  $("run").addEventListener("click", () => start("run"));
  $("final").addEventListener("click", () => start("final"));
  showKind();
  load().catch((e) => document.body.prepend(para("status bad", e.message)));
});

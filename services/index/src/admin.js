"use strict";
// Everything from reports goes in with textContent: it is text from strangers.
let token = "";
const $ = (id) => document.getElementById(id);
const server = (id) => "servers/" + encodeURIComponent(id);

async function call(method, path, body) {
  const headers = { authorization: "Bearer " + token };
  if (body) headers["content-type"] = "application/json";
  const response = await fetch(path, { method, headers, body: body && JSON.stringify(body) });
  if (!response.ok) {
    let code = String(response.status);
    try { code = (await response.json()).error; } catch (_) { /* not JSON */ }
    throw new Error(code);
  }
  return response.status === 204 ? null : response.json();
}

function cell(row, text) {
  const td = row.insertCell();
  td.textContent = text;
  return td;
}

function button(td, label, action) {
  const b = document.createElement("button");
  b.textContent = label;
  b.addEventListener("click", () => run(action));
  td.append(b);
}

async function run(action) {
  try {
    await action();
    await load();
  } catch (error) {
    $("status").textContent = "Failed: " + error.message;
  }
}

function remove(id, action) {
  return async () => {
    const reason = prompt(`Why ${action} this server? The server's owner sees this.`);
    if (reason) await call("PUT", server(id), { action, reason });
  };
}

async function load() {
  const { reports, sanctions } = await call("GET", "reports");
  const list = $("reports").tBodies[0];
  list.replaceChildren();
  for (const report of reports) {
    const row = list.insertRow();
    cell(row, new Date(report.created_at).toLocaleString());
    cell(row, `${report.name}\n${report.address}\n${report.server_id}`);
    cell(row, report.reason);
    cell(row, report.note);
    const actions = row.insertCell();
    button(actions, "Delist", remove(report.server_id, "delist"));
    button(actions, "Block", remove(report.server_id, "block"));
    button(actions, "Dismiss", () => call("DELETE", server(report.server_id) + "/reports"));
  }
  const removed = $("sanctions").tBodies[0];
  removed.replaceChildren();
  for (const sanction of sanctions) {
    const row = removed.insertRow();
    cell(row, new Date(sanction.created_at).toLocaleString());
    cell(row, sanction.server_id);
    cell(row, sanction.host ? `${sanction.action} (${sanction.host})` : sanction.action);
    cell(row, sanction.reason);
    button(row.insertCell(), "Restore", () => call("DELETE", server(sanction.server_id)));
  }
  $("status").textContent = `${reports.length} reports, ${sanctions.length} removed servers.`;
}

$("login").addEventListener("submit", (event) => {
  event.preventDefault();
  token = $("token").value;
  run(async () => {});
});

// The zephyr web app (DESIGN §4.3.3): chat, approvals, settings and devices over the attach
// protocol on a WebSocket. Plain ES2020, no build step. Everything the daemon sends is put on
// the page as text (`textContent`), never as markup.
"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const state = {
    ws: null,
    next: 1,
    pending: new Map(), // request id → {resolve, reject}
    backoff: 1000,
    timer: null,
    snapshot: null,
    proposals: [],
    sheet: null, // the open card: {id, card (as received), hash}
    held: null, // a card to open once the proposals are in (`#held/ID`, a notification tap)
    turn: null, // the running turn's tool line
    live: null, // the assistant message being streamed
    pushKey: null,
    models: null,
  };

  // ---- small helpers ----

  function el(tag, cls, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = clean(String(text));
    return node;
  }

  // Control and direction marks are dropped for display, so text cannot hide or reorder what
  // is on a card. Hashes are always made from the text as received.
  function clean(text) {
    return text.replace(/[\u0000-\u0009\u000B-\u001F\u007F-\u009F‎‏‪-‮⁦-⁩]/g, "");
  }

  function b64(buffer) {
    let text = "";
    for (const byte of new Uint8Array(buffer)) text += String.fromCharCode(byte);
    return btoa(text).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  function unb64(text) {
    const raw = atob(text.replace(/-/g, "+").replace(/_/g, "/"));
    return Uint8Array.from(raw, (c) => c.charCodeAt(0));
  }

  async function sha256hex(text) {
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
    return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("");
  }

  function span(secs) {
    const s = Math.max(0, Math.floor(secs));
    if (s < 60) return s + "s";
    if (s < 3600) return Math.floor(s / 60) + "m";
    if (s < 86400) return Math.floor(s / 3600) + "h";
    return Math.floor(s / 86400) + "d";
  }

  function clock(unix) {
    return new Date(unix * 1000).toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });
  }

  async function api(method, path, body) {
    const init = { method, credentials: "same-origin", headers: {} };
    if (body !== undefined) {
      init.headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const res = await fetch(path, init);
    let data = {};
    try {
      data = await res.json();
    } catch (_) {
      // no body
    }
    return { ok: res.ok, status: res.status, body: data };
  }

  const said = (error) => (error && error.message) || "that did not work; try again";

  // ---- the socket ----

  function rpc(method, params) {
    return new Promise((resolve, reject) => {
      const ws = state.ws;
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        reject({ message: "not connected; trying again" });
        return;
      }
      const id = state.next++;
      state.pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params: params || {} }));
    });
  }

  function retry() {
    clearTimeout(state.timer);
    state.timer = setTimeout(connect, state.backoff);
    state.backoff = Math.min(state.backoff * 2, 30000);
  }

  async function connect() {
    clearTimeout(state.timer);
    if (document.hidden || state.ws) return;
    // A browser cannot read a refused upgrade's status, so the session is checked first.
    let key;
    try {
      key = await api("GET", "/api/push/key");
    } catch (_) {
      retry();
      return;
    }
    if (key.status === 401) {
      showSignin("");
      return;
    }
    state.pushKey = key.ok ? key.body.key : null;
    showApp();
    if (state.ws) return;
    const scheme = location.protocol === "https:" ? "wss://" : "ws://";
    const ws = new WebSocket(scheme + location.host + "/api/ws");
    state.ws = ws;
    ws.onopen = async () => {
      state.backoff = 1000;
      try {
        const sub = await rpc("zephyr.subscribe", {});
        renderHistory(sub.messages || []);
        await loadProposals();
        syncPush();
      } catch (_) {
        // the close handler reconnects
      }
    };
    ws.onmessage = (frame) => onFrame(frame.data);
    ws.onclose = () => {
      if (state.ws === ws) state.ws = null;
      for (const waiter of state.pending.values()) waiter.reject({ message: "the connection closed; try again" });
      state.pending.clear();
      if (!document.hidden) retry();
    };
  }

  function onFrame(text) {
    let frame;
    try {
      frame = JSON.parse(text);
    } catch (_) {
      return;
    }
    if (frame.id !== undefined && frame.id !== null) {
      const waiter = state.pending.get(frame.id);
      state.pending.delete(frame.id);
      if (!waiter) return;
      if (frame.error) waiter.reject(frame.error);
      else waiter.resolve(frame.result);
    } else if (frame.method === "event" && frame.params) {
      onEvent(frame.params.event || {});
    }
  }

  // A hidden app closes its socket, so replies are pushed instead of sent to a page nobody
  // is looking at; it reconnects when it is shown again.
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      clearTimeout(state.timer);
      if (state.ws) state.ws.close();
    } else {
      state.backoff = 1000;
      connect();
    }
  });

  // ---- sign-in ----

  function showSignin(note) {
    $("app").hidden = true;
    $("signin").hidden = false;
    $("signin-note").textContent = note;
  }

  function showApp() {
    $("signin").hidden = true;
    $("app").hidden = false;
  }

  function assertion(credential) {
    const r = credential.response;
    return {
      id: b64(credential.rawId),
      clientDataJSON: b64(r.clientDataJSON),
      authenticatorData: b64(r.authenticatorData),
      signature: b64(r.signature),
      userHandle: r.userHandle ? b64(r.userHandle) : null,
    };
  }

  // What `navigator.credentials.get` takes, from the daemon's options (base64url → bytes).
  function request(options) {
    const publicKey = {
      challenge: unb64(options.challenge),
      rpId: options.rpId,
      timeout: options.timeout,
      userVerification: options.userVerification,
    };
    if (options.allowCredentials) {
      publicKey.allowCredentials = options.allowCredentials.map((c) => ({ type: c.type, id: unb64(c.id) }));
    }
    return { publicKey };
  }

  async function signIn() {
    $("signin-note").textContent = "";
    try {
      const start = await api("POST", "/api/login/start", {});
      if (!start.ok) throw { message: start.body.error };
      const credential = await navigator.credentials.get(request(start.body.publicKey));
      const body = Object.assign({ challengeId: start.body.challengeId }, assertion(credential));
      const done = await api("POST", "/api/login/finish", body);
      if (!done.ok) throw { message: done.body.error };
      connect();
    } catch (error) {
      $("signin-note").textContent = said(error);
    }
  }

  async function enrol() {
    $("signin-note").textContent = "";
    try {
      const start = await api("POST", "/api/enrol/start", { code: $("enrol-code").value });
      if (!start.ok) throw { message: start.body.error };
      const o = start.body.publicKey;
      const credential = await navigator.credentials.create({
        publicKey: {
          rp: o.rp,
          user: { id: unb64(o.user.id), name: o.user.name, displayName: o.user.displayName },
          challenge: unb64(o.challenge),
          pubKeyCredParams: o.pubKeyCredParams,
          authenticatorSelection: o.authenticatorSelection,
          attestation: o.attestation,
          timeout: o.timeout,
          excludeCredentials: o.excludeCredentials.map((c) => ({ type: c.type, id: unb64(c.id) })),
        },
      });
      // The browser reads the attestation, so the daemon parses no CBOR.
      const r = credential.response;
      const body = {
        challengeId: start.body.challengeId,
        id: b64(credential.rawId),
        clientDataJSON: b64(r.clientDataJSON),
        authenticatorData: b64(r.getAuthenticatorData()),
        publicKey: b64(r.getPublicKey()),
        alg: r.getPublicKeyAlgorithm(),
      };
      const name = $("enrol-name").value.trim();
      if (name) body.name = name;
      const done = await api("POST", "/api/enrol/finish", body);
      if (!done.ok) throw { message: done.body.error };
      $("enrol-code").value = "";
      connect();
    } catch (error) {
      $("signin-note").textContent = said(error);
    }
  }

  // ---- the transcript ----

  function textOf(message) {
    if (!message) return "";
    if (typeof message.text === "string") return message.text; // a user turn, tag removed
    const content = message.content;
    if (typeof content === "string") return content;
    if (!Array.isArray(content)) return "";
    return content.filter((part) => part && part.type === "text").map((part) => part.text || "").join("");
  }

  const role = (message) => (message && (message.kind || message.role)) || "";

  function scrollDown() {
    const box = $("transcript");
    box.scrollTop = box.scrollHeight;
  }

  // Text with ``` fences: prose as paragraphs, code in <pre> with a copy button.
  function fill(node, text) {
    node.replaceChildren();
    const parts = text.split(/^```.*$/m);
    parts.forEach((part, i) => {
      if (i % 2 === 0) {
        if (part.trim()) node.append(el("p", "prose", part.trim()));
        return;
      }
      const code = part.replace(/^\n/, "").replace(/\n$/, "");
      const block = el("div", "code");
      const copy = el("button", "copy", "copy");
      copy.type = "button";
      copy.addEventListener("click", () => navigator.clipboard.writeText(code));
      block.append(copy, el("pre", "", code));
      node.append(block);
    });
  }

  function addUser(message) {
    const row = el("div", "msg user");
    const origin = message.origin || {};
    row.append(el("div", "band", origin.channel || "you"));
    row.append(el("p", "prose", textOf(message)));
    $("transcript").append(row);
    scrollDown();
  }

  function addAssistant(text) {
    const row = el("div", "msg agent");
    fill(row, text);
    $("transcript").append(row);
    scrollDown();
    return row;
  }

  function addNotice(text, command) {
    const row = el("div", "msg notice");
    if (command) row.append(el("div", "band", command));
    row.append(el("pre", "", text));
    $("transcript").append(row);
    scrollDown();
  }

  function renderHistory(messages) {
    $("transcript").replaceChildren();
    state.live = null;
    state.turn = null;
    for (const message of messages) {
      const text = textOf(message);
      if (role(message) === "user") addUser(message);
      else if (role(message) === "assistant" && text.trim()) addAssistant(text);
    }
  }

  // A turn's tools, folded into one line: `▸ 3 tools  bash, read ×2 ✓ 4.1s`.
  function toolLine(done) {
    const turn = state.turn;
    if (!turn) return;
    const counts = new Map();
    for (const name of turn.names) counts.set(name, (counts.get(name) || 0) + 1);
    const names = Array.from(counts, ([name, n]) => (n > 1 ? name + " ×" + n : name)).join(", ");
    let line = "▸ " + turn.names.length + (turn.names.length === 1 ? " tool  " : " tools  ") + names;
    if (done) line += (turn.failed ? " ✗ " : " ✓ ") + ((Date.now() - turn.started) / 1000).toFixed(1) + "s";
    turn.node.textContent = clean(line);
  }

  function onEvent(event) {
    switch (event.type) {
      case "zephyr_status":
        onSnapshot(event.snapshot || {});
        break;
      case "zephyr_reply":
        addNotice(String(event.text || ""), event.command);
        if (String(event.command || "").startsWith("/goal") && !$("goals").hidden) loadGoals();
        // A reply to an approval or a `/web` command changes what the lists show.
        loadProposals();
        break;
      case "agent_start":
        state.turn = null;
        state.live = null;
        break;
      case "message_start":
      case "message_update":
      case "message_end": {
        const message = event.message;
        if (role(message) === "user") {
          if (event.type === "message_end") addUser(message);
          break;
        }
        if (role(message) !== "assistant") break;
        const text = textOf(message);
        if (!state.live && text.trim()) state.live = addAssistant(text);
        else if (state.live) fill(state.live, text);
        if (event.type === "message_end") state.live = null;
        scrollDown();
        break;
      }
      case "tool_execution_start":
        if (!state.turn) {
          state.turn = { node: el("div", "tools"), names: [], started: Date.now(), failed: false };
          $("transcript").append(state.turn.node);
        }
        state.turn.names.push(String(event.toolName || "tool"));
        toolLine(false);
        scrollDown();
        break;
      case "tool_execution_end":
        if (state.turn && event.isError) state.turn.failed = true;
        break;
      case "agent_settled":
      case "agent_end":
        toolLine(true);
        state.turn = null;
        state.live = null;
        break;
      default:
        break;
    }
  }

  // ---- the status line, the queue, the composer ----

  function statusText() {
    const snap = state.snapshot || {};
    const run = snap.run || {};
    if (!run.busy) return snap.model ? "idle · " + snap.model : "idle";
    const secs = run.since_unix_ms ? Math.max(0, Math.floor((Date.now() - run.since_unix_ms) / 1000)) : 0;
    const parts = ["working", Math.floor(secs / 60) + ":" + String(secs % 60).padStart(2, "0")];
    if (run.current_tool) parts.push(run.current_tool);
    if (run.tools_done) parts.push(run.tools_done + (run.tools_done === 1 ? " tool" : " tools"));
    return parts.join(" · ");
  }

  function onSnapshot(snapshot) {
    const before = state.snapshot ? state.snapshot.proposals : -1;
    state.snapshot = snapshot;
    $("status").textContent = clean(statusText());
    $("paused").hidden = !snapshot.paused;
    const waiting = snapshot.proposals || 0;
    $("waiting").hidden = waiting === 0;
    $("waiting").textContent = waiting + " waiting";
    $("stop").hidden = !(snapshot.run && snapshot.run.busy);
    const mine = (snapshot.queue || []).filter((q) => q.channel === "web");
    $("queue").hidden = mine.length === 0;
    $("queue-text").textContent = clean(mine.length + " waiting: " + (mine[0] ? mine[0].preview : ""));
    renderUpcoming();
    if (waiting !== before) loadProposals();
  }

  setInterval(() => {
    if (state.snapshot && state.snapshot.run && state.snapshot.run.busy) {
      $("status").textContent = clean(statusText());
    }
  }, 1000);

  async function sendDraft() {
    const text = $("draft").value.trim();
    if (!text) return;
    const command = { type: "prompt", content: text };
    if ($("follow").checked) command.followUp = true;
    try {
      await rpc("send", { sessionId: "main", command });
      $("draft").value = "";
    } catch (error) {
      addNotice(said(error));
    }
  }

  // A command sent for the owner (`/web rm NAME`): its reply comes back as a notice.
  function sendCommand(text) {
    return rpc("send", { sessionId: "main", command: { type: "prompt", content: text } });
  }

  async function takeBack() {
    try {
      const back = await rpc("zephyr.dequeue", {});
      const texts = (back.texts || []).join("\n");
      if (texts) $("draft").value = $("draft").value ? $("draft").value + "\n" + texts : texts;
    } catch (error) {
      addNotice(said(error));
    }
  }

  // ---- the drawer: approvals and upcoming ----

  function openDrawer(tab) {
    $("drawer").hidden = false;
    for (const name of ["approvals", "activity", "upcoming"]) $(name).hidden = name !== tab;
    for (const button of document.querySelectorAll("#drawer [data-tab]")) {
      button.setAttribute("aria-selected", String(button.dataset.tab === tab));
    }
    if (tab === "activity") loadActivity();
  }

  // ---- the drawer: activity (the daemon's own record) and the standing grants ----

  // A grant's revoke only takes away, so it goes as the owner's own command. One the daemon
  // cannot offer (`revoke` null) says "set … back by hand" in its line, and gets no button.
  async function loadGrants(rows) {
    const grants = await rpc("zephyr.activity", { kind: "grants" });
    for (const grant of grants.entries || []) {
      const item = el("li", "row");
      item.append(el("span", "what", grant.line));
      if (typeof grant.revoke === "string" && grant.revoke.startsWith("/")) {
        const revoke = el("button", "", "Revoke");
        revoke.type = "button";
        revoke.addEventListener("click", async () => {
          if (!confirm("Send " + clean(grant.revoke) + " ?")) return;
          await sendCommand(grant.revoke).catch((error) => addNotice(said(error)));
          loadActivity();
        });
        item.append(revoke);
      }
      rows.append(item);
    }
  }

  // One page of the record; `before` is the last page's `next`. The "older" row asks for the
  // next page when it scrolls into view, or on a tap.
  async function loadPage(rows, before) {
    const page = await rpc("zephyr.activity", before ? { before } : {});
    for (const entry of page.entries || []) {
      rows.append(el("li", entry.kind === "tool" ? "row tool" : "row", entry.line));
    }
    if (!page.next) return;
    const item = el("li");
    const older = el("button", "row", "older");
    older.type = "button";
    const more = () => {
      item.remove();
      loadPage(rows, page.next).catch(() => null);
    };
    older.addEventListener("click", more);
    item.append(older);
    rows.append(item);
    if ("IntersectionObserver" in window) {
      const seen = new IntersectionObserver((hits) => {
        if (hits.some((hit) => hit.isIntersecting) && item.isConnected) {
          seen.disconnect();
          more();
        }
      });
      seen.observe(older);
    }
  }

  async function loadActivity() {
    const rows = $("activity");
    rows.replaceChildren();
    try {
      await loadGrants(rows);
      await loadPage(rows, null);
      if (!rows.firstChild) rows.append(el("li", "empty", "nothing in the last 24 hours"));
    } catch (error) {
      // A daemon without the record: no tab.
      if (error && error.code === -32601) {
        document.querySelector('#drawer [data-tab="activity"]').hidden = true;
        openDrawer("approvals");
      } else {
        rows.append(el("li", "empty", said(error)));
      }
    }
  }

  function firstLine(card) {
    return String(card || "").split("\n")[0];
  }

  async function loadProposals() {
    let list;
    try {
      list = (await rpc("zephyr.proposals", {})).proposals || [];
    } catch (_) {
      return;
    }
    state.proposals = list;
    const rows = $("approvals");
    rows.replaceChildren();
    const waiting = list.filter((p) => p.status !== "scheduled");
    if (waiting.length === 0) rows.append(el("li", "empty", "nothing waits for you"));
    for (const p of waiting) {
      const button = el("button", "row");
      button.type = "button";
      const what = p.class ? p.class + " · " + (p.summary || "") : firstLine(p.card);
      button.append(el("span", "what", "#" + p.id + " " + what));
      button.append(el("span", "when", span(p.age_secs) + " ago · expires in " + span(p.expires_in_secs)));
      button.addEventListener("click", () => openSheet(p));
      const item = el("li");
      item.append(button);
      rows.append(item);
    }
    renderUpcoming();
    await checkSheet();
    if (state.held !== null) {
      const held = list.find((p) => p.id === state.held);
      state.held = null;
      if (held) openSheet(held);
    }
  }

  function renderUpcoming() {
    const rows = $("upcoming");
    rows.replaceChildren();
    for (const p of state.proposals.filter((p) => p.status === "scheduled")) {
      const button = el("button", "row");
      button.type = "button";
      button.append(el("span", "what", "#" + p.id + " " + (p.class || "") + " · " + (p.summary || firstLine(p.card))));
      button.append(el("span", "when", "goes by itself " + (p.due_at ? clock(p.due_at) : "soon")));
      button.addEventListener("click", () => openSheet(p));
      const item = el("li");
      item.append(button);
      rows.append(item);
    }
    for (const job of (state.snapshot && state.snapshot.jobs) || []) {
      const item = el("li", "row");
      item.append(el("span", "what", job.name), el("span", "when", clock(job.next_unix)));
      rows.append(item);
    }
    if (!rows.firstChild) rows.append(el("li", "empty", "nothing is scheduled"));
  }

  // ---- the card sheet ----

  // What the sheet shows and a passkey signs: the card without its countdown, which changes
  // by itself and is shown apart, in the title.
  const signedText = (p) => String(p.signed !== undefined ? p.signed : p.card || "");

  async function openSheet(p) {
    const text = signedText(p);
    // The hash is of the text as received: what the daemon will compare.
    state.sheet = { id: p.id, hash: await sha256hex(text), strong: !!p.strong };
    const lines = text.split("\n");
    const at = lines.findIndex((line) => line.startsWith("the agent says: "));
    const reason = at >= 0 ? lines.splice(at, 1)[0] : null;
    const left = p.status === "scheduled" ? "" : " · expires in " + span(p.expires_in_secs);
    $("sheet-title").textContent = "#" + p.id + left;
    $("sheet-card").textContent = clean(lines.join("\n"));
    $("sheet-reason").hidden = reason === null;
    $("sheet-reason").open = false;
    $("sheet-reason-text").textContent = clean(reason || "");
    $("sheet-note").textContent = "";
    $("sheet").classList.remove("stale");
    $("sheet-once").hidden = !p.once;
    $("sheet-cancel").hidden = p.status !== "scheduled";
    for (const id of ["sheet-ok", "sheet-once", "sheet-no", "sheet-cancel"]) $(id).disabled = false;
    if (!$("sheet").open) $("sheet").showModal();
  }

  function staleSheet(note) {
    $("sheet").classList.add("stale");
    $("sheet-note").textContent = clean(note);
    for (const id of ["sheet-ok", "sheet-once", "sheet-no", "sheet-cancel"]) $(id).disabled = true;
  }

  // An open sheet is never rebuilt. If its proposal is gone or its card differs, it greys out.
  async function checkSheet() {
    const sheet = state.sheet;
    if (!sheet || !$("sheet").open || $("sheet").classList.contains("stale")) return;
    const now = state.proposals.find((p) => p.id === sheet.id);
    const same = now && (await sha256hex(signedText(now))) === sheet.hash;
    if (!same && state.sheet === sheet) staleSheet("this card changed or was handled; close and look again");
  }

  // Signs `op` with this session's passkey: the challenge commits to what is on screen.
  async function sign(op) {
    const made = await rpc("zephyr.web_challenge", op);
    const credential = await navigator.credentials.get(request(made.publicKey));
    return { challengeId: made.challengeId, assertion: assertion(credential) };
  }

  async function decide(verb) {
    const sheet = state.sheet;
    if (!sheet) return;
    $("sheet-note").textContent = "";
    try {
      let params = { id: sheet.id, verb };
      if (verb === "ok" || verb === "once") {
        const signed = await sign({ op: "decide", id: sheet.id, verb, card_sha256: sheet.hash });
        params = Object.assign(params, signed);
      }
      const done = await rpc("zephyr.decide", params);
      if (done.ok) staleSheet(String(done.text || "done"));
      else $("sheet-note").textContent = clean(String(done.text || ""));
    } catch (error) {
      $("sheet-note").textContent = clean(said(error));
    }
    loadProposals();
  }

  // ---- settings ----

  function editor(row) {
    let kind = row.kind || {};
    if (kind.type === "optional" && kind.inner) kind = kind.inner;
    let options = null;
    if (kind.type === "bool") options = ["true", "false"];
    else if (Array.isArray(kind.options)) options = kind.options.map(String);
    else if (kind.type === "model" && state.models) options = state.models;
    if (!options) {
      const input = el("input");
      input.value = row.value === null || row.value === undefined ? "" : String(row.value);
      input.setAttribute("aria-label", row.label);
      return input;
    }
    const select = el("select");
    select.setAttribute("aria-label", row.label);
    const current = row.value === null || row.value === undefined ? "" : String(row.value);
    for (const option of options.includes(current) ? options : [current].concat(options)) {
      const item = el("option", "", option);
      item.value = option;
      item.selected = option === current;
      select.append(item);
    }
    return select;
  }

  async function saveSetting(row, value, until, note) {
    note.textContent = "";
    try {
      let params = { key: row.key, value };
      if (until) params.until = until;
      // A protected setting is always signed: unsigned, the cookie alone could not loosen it.
      if (row.strong) params = Object.assign(params, await sign(Object.assign({ op: "set" }, params)));
      const done = await rpc("zephyr.settings_set", params);
      if (done.outcome === "changed") {
        loadSettings();
      } else if (done.outcome === "proposed") {
        note.textContent = "proposed as #" + done.id;
      } else {
        note.textContent = clean(String(done.reason || "refused"));
      }
    } catch (error) {
      note.textContent = clean(said(error));
    }
  }

  async function loadSettings() {
    const list = $("settings-list");
    let settings;
    try {
      settings = await rpc("zephyr.settings", {});
      if (!state.models) {
        const models = await rpc("zephyr.models", {}).catch(() => null);
        const all = ((models && models.accounts) || []).flatMap((a) => a.models || []);
        state.models = Array.from(new Set(all));
      }
    } catch (error) {
      $("settings-note").textContent = clean(said(error));
      return;
    }
    $("settings-note").textContent = "";
    list.replaceChildren();
    for (const section of settings.sections || []) {
      list.append(el("h3", "", section.name));
      for (const row of section.settings || []) {
        const box = el("details", "setting");
        const head = el("summary");
        const value = row.value === null || row.value === undefined ? "unset" : row.value;
        head.append(el("span", "what", row.label), el("span", "when", value + " · " + row.source));
        box.append(head, el("p", "hint", row.key + ": " + (row.help || "")));
        if (row.access !== "read-only") {
          const input = editor(row);
          const until = el("input");
          until.placeholder = "for 2h (optional)";
          until.setAttribute("aria-label", "time limit");
          const note = el("p", "note");
          note.setAttribute("role", "status");
          const save = el("button", "primary", row.strong ? "Save with passkey" : "Save");
          save.type = "button";
          save.addEventListener("click", () => saveSetting(row, input.value, until.value.trim(), note));
          const fields = el("div", "buttons");
          fields.append(input, until, save);
          box.append(fields, note);
        }
        list.append(box);
      }
    }
  }

  // ---- facts, vault, devices ----

  async function loadList(method, key, into, line) {
    const rows = $(into);
    rows.replaceChildren();
    try {
      const got = await rpc(method, {});
      for (const item of got[key] || []) rows.append(el("li", "row", line(item)));
      if (!rows.firstChild) rows.append(el("li", "empty", "none"));
    } catch (error) {
      rows.append(el("li", "empty", said(error)));
    }
  }

  const loadFacts = () =>
    loadList("zephyr.facts_list", "facts", "facts-list", (f) => {
      const allow = (f.allow || []).length ? " · allowed: " + f.allow.join(", ") : "";
      return f.name + " · " + f.kind + allow;
    });

  const loadVault = () =>
    loadList("zephyr.vault_list", "entries", "vault-list", (e) => {
      const where = e.site ? " · " + e.site : "";
      const programs = (e.programs || []).length ? " · " + e.programs.join(", ") : "";
      return e.name + " · " + e.kind + where + (e.granted ? " · granted" : " · not granted") + programs;
    });

  async function loadDevices() {
    let devices;
    try {
      devices = await rpc("zephyr.web_devices", {});
    } catch (error) {
      $("devices-note").textContent = clean(said(error));
      return;
    }
    const used = (at) => (at ? "used " + span(Date.now() / 1000 - at) + " ago" : "never used");
    const keys = $("passkeys-list");
    keys.replaceChildren();
    for (const key of devices.passkeys || []) {
      const item = el("li", "row");
      const remove = el("button", "", "Remove");
      remove.type = "button";
      remove.addEventListener("click", async () => {
        if (!confirm("Remove passkey " + key.name + " and sign its devices out?")) return;
        await sendCommand("/web rm " + key.name).catch((error) => addNotice(said(error)));
        loadDevices();
      });
      item.append(el("span", "what", key.name), el("span", "when", used(key.last_used_at)), remove);
      keys.append(item);
    }
    const sessions = $("sessions-list");
    sessions.replaceChildren();
    for (const s of devices.sessions || []) {
      const line = (s.passkey || "?") + (s.this ? " (this one)" : "") + " · seen " + span(Date.now() / 1000 - s.seen_at) + " ago";
      sessions.append(el("li", "row", line));
    }
    const push = $("push-list");
    push.replaceChildren();
    for (const p of devices.push || []) push.append(el("li", "row", (p.passkey || "?") + " · since " + clock(p.created_at)));
    if (!push.firstChild) push.append(el("li", "empty", "no device gets notifications"));
    const sub = await subscription();
    $("push-on").hidden = !!sub;
    $("push-off").hidden = !sub;
  }

  // ---- goals ----

  // Read with `zephyr.goals`; changed with the owner's `/goal` commands, whose replies come
  // back as notices and reload the list. A done goal has no number, so nothing acts on it.
  async function goalCommand(text) {
    $("goals-note").textContent = "";
    await sendCommand(text).catch((error) => {
      $("goals-note").textContent = clean(said(error));
    });
  }

  async function loadGoals() {
    const rows = $("goals-list");
    let goals;
    try {
      goals = (await rpc("zephyr.goals", {})).goals || [];
    } catch (error) {
      // A daemon without goals: no screen.
      if (error && error.code === -32601) {
        document.querySelector('#screens [data-screen="goals"]').hidden = true;
        showScreen("chat");
      } else {
        $("goals-note").textContent = clean(said(error));
      }
      return;
    }
    rows.replaceChildren();
    for (const goal of goals) {
      const open = !goal.done && Number.isInteger(goal.n);
      const item = el("li", open ? "row" : "row done");
      item.append(el("span", "what", (open ? goal.n + ". " : "") + goal.text));
      if (open) {
        for (const [label, verb] of [["Done", "done"], ["Remove", "rm"]]) {
          const button = el("button", "", label);
          button.type = "button";
          button.addEventListener("click", () => goalCommand("/goal " + verb + " " + goal.n));
          item.append(button);
        }
      }
      rows.append(item);
    }
    if (!rows.firstChild) rows.append(el("li", "empty", "no goals yet"));
  }

  function addGoal() {
    // One line: the command is the whole message.
    const text = $("goal-text").value.replace(/\s+/g, " ").trim();
    if (!text) return;
    $("goal-text").value = "";
    goalCommand("/goal add " + text);
  }

  // ---- notifications ----

  async function subscription() {
    if (!("serviceWorker" in navigator) || !("PushManager" in window)) return null;
    const registration = await navigator.serviceWorker.getRegistration();
    return registration ? registration.pushManager.getSubscription() : null;
  }

  async function subscribe() {
    const registration = await navigator.serviceWorker.ready;
    const sub = await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: unb64(state.pushKey),
    });
    const json = sub.toJSON();
    const keys = { p256dh: json.keys.p256dh.replace(/=+$/, ""), auth: json.keys.auth.replace(/=+$/, "") };
    const saved = await api("POST", "/api/push", { endpoint: json.endpoint, keys });
    if (!saved.ok) {
      await sub.unsubscribe();
      throw { message: saved.body.error || "this push service is not one zephyr sends to" };
    }
  }

  async function unsubscribe() {
    const sub = await subscription();
    if (!sub) return;
    await api("DELETE", "/api/push", { endpoint: sub.endpoint });
    await sub.unsubscribe();
  }

  // Asked for on the tap: iOS gives permission only then, and only to a Home Screen app.
  async function pushOn() {
    const note = $("devices-note");
    note.textContent = "";
    try {
      if (!("serviceWorker" in navigator) || !("PushManager" in window) || !state.pushKey) {
        throw { message: "no notifications here; on an iPhone, add the app to the Home Screen first" };
      }
      if ((await Notification.requestPermission()) !== "granted") {
        throw { message: "notifications are blocked for this site" };
      }
      await subscribe();
    } catch (error) {
      note.textContent = clean(said(error));
    }
    loadDevices();
  }

  // On each open: a subscription made with another key (the daemon's was remade) is renewed.
  async function syncPush() {
    try {
      const sub = await subscription();
      if (!sub || !state.pushKey) return;
      const key = sub.options && sub.options.applicationServerKey;
      if (key && b64(key) !== state.pushKey) {
        await unsubscribe();
        await subscribe();
      }
    } catch (_) {
      // the Devices screen shows the state
    }
  }

  async function signOut() {
    await unsubscribe().catch(() => null);
    await api("POST", "/api/logout", {});
    if (state.ws) state.ws.close();
    showSignin("signed out");
  }

  // ---- screens and wiring ----

  const loaders = {
    settings: loadSettings,
    facts: loadFacts,
    vault: loadVault,
    goals: loadGoals,
    devices: loadDevices,
  };

  function showScreen(name) {
    for (const screen of document.querySelectorAll(".screen")) screen.hidden = screen.id !== name;
    $("screens").hidden = true;
    if (loaders[name]) loaders[name]();
  }

  function wire() {
    $("signin-go").addEventListener("click", signIn);
    $("enrol-go").addEventListener("click", enrol);
    $("send").addEventListener("click", sendDraft);
    $("draft").addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
        event.preventDefault();
        sendDraft();
      }
    });
    $("stop").addEventListener("click", () => {
      rpc("send", { sessionId: "main", command: { type: "abort" } }).catch((error) => addNotice(said(error)));
    });
    $("queue-back").addEventListener("click", takeBack);
    $("menu").addEventListener("click", () => {
      $("screens").hidden = !$("screens").hidden;
    });
    for (const button of document.querySelectorAll("#screens [data-screen]")) {
      button.addEventListener("click", () => showScreen(button.dataset.screen));
    }
    // The drawer opens on Approvals when something waits.
    const drawer = () => openDrawer(state.snapshot && state.snapshot.proposals > 0 ? "approvals" : "upcoming");
    $("status").addEventListener("click", drawer);
    $("waiting").addEventListener("click", () => openDrawer("approvals"));
    $("drawer-close").addEventListener("click", () => {
      $("drawer").hidden = true;
    });
    for (const button of document.querySelectorAll("#drawer [data-tab]")) {
      button.addEventListener("click", () => openDrawer(button.dataset.tab));
    }
    $("sheet-ok").addEventListener("click", () => decide("ok"));
    $("sheet-once").addEventListener("click", () => decide("once"));
    $("sheet-no").addEventListener("click", () => decide("no"));
    $("sheet-cancel").addEventListener("click", () => decide("cancel"));
    $("sheet-close").addEventListener("click", () => $("sheet").close());
    $("sheet").addEventListener("close", () => {
      state.sheet = null;
    });
    $("goal-add").addEventListener("click", addGoal);
    $("push-on").addEventListener("click", pushOn);
    $("push-off").addEventListener("click", () => unsubscribe().then(loadDevices, loadDevices));
    $("signout").addEventListener("click", signOut);
  }

  function boot() {
    wire();
    // `#enrol=CODE` fills the code and leaves the address bar; `#held/ID` opens that card.
    const hash = location.hash;
    if (hash.startsWith("#enrol=")) {
      $("enrol-code").value = decodeURIComponent(hash.slice(7));
      $("enrol").open = true;
      history.replaceState(null, "", "/");
    } else if (hash.startsWith("#held/")) {
      state.held = Number(hash.slice(6)) || null;
      history.replaceState(null, "", "/");
    }
    if ("serviceWorker" in navigator) {
      navigator.serviceWorker.register("/sw.js").catch(() => null);
      navigator.serviceWorker.addEventListener("message", (event) => {
        if (event.data && event.data.held) {
          state.held = Number(event.data.held) || null;
          loadProposals();
        }
      });
    }
    connect();
  }

  boot();
})();

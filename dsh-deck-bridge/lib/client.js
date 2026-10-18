// dsh-deck-bridge web client.
//
// The desktop companion switches the active DSH session without touching the
// mouse. Two transports reach this page-side half:
//
//   GET /api/deck/next  — an authenticated long poll for commands from the
//     host plugin. Works in a background tab without browser automation rights.
//   <base>#deck-<nonce>-<session-id>  — a hash-only URL rewrite, kept as the
//     fallback for setups where the companion may still script the browser.
//
// The nonce is lowercase hex and the session id starts with "session-", so
// the two parts can never be confused when splitting.

window.__ModuleLoader__.load({
  id: "dsh-deck-bridge",
  factory: function (require) {
    var module = { exports: {} };

    var HANDOFF = /^#deck-([0-9a-f]+)-(session-[A-Za-z0-9_-]+)$/;
    var SESSION_ID = /^session-[A-Za-z0-9_-]{1,72}$/;
    var NONCE = /^[0-9a-f]{1,32}$/;

    module.exports.inject = ["sessions", "layout"];

    // DSH moved session navigation off `sessions` onto the `uiWorkspace`
    // service in 0.1.7, so both names are probed at call time.
    module.exports.apply = function (ctx) {
      var sessions = ctx.get("sessions");
      if (sessions === undefined) return;
      var lastOpenError = "";

      var openSession = function (sessionId) {
        try {
          lastOpenError = "";
          var workspace = ctx.get("uiWorkspace");
          if (workspace && typeof workspace.openSession === "function") {
            workspace.openSession(sessionId);
          } else if (sessions && typeof sessions.open === "function") {
            sessions.open(sessionId);
          } else {
            throw new Error("no session navigation API (uiWorkspace.openSession / sessions.open)");
          }
          return true;
        } catch (err) {
          lastOpenError = String(err && err.message || err).slice(0, 160);
          console.error("dsh-deck-bridge open:", err);
          return false;
        } finally {
          // DSH can show another sidebar panel while a session is selected.
          // The normal workspace navigation clears it when opening a chat.
          // Best-effort: a panel failure must not nack an accepted open.
          try {
            ctx.layout.selectPanel(null);
          } catch (err) {
            console.error("dsh-deck-bridge selectPanel:", err);
          }
        }
      };

      var consume = function () {
        var match = HANDOFF.exec(window.location.hash || "");
        if (match === null) return;
        var sessionId = match[2];
        // Clear the handoff only after the page accepted the session.
        if (!openSession(sessionId)) return;
        try {
          window.history.replaceState(
            null,
            "",
            window.location.pathname + window.location.search
          );
        } catch (err) {
          console.error("dsh-deck-bridge replaceState:", err);
        }
      };

      window.addEventListener("hashchange", consume);
      // Pick up a handoff that arrived before this module applied.
      consume();

      // Each response closes normally, then the page immediately requests the
      // next one. This avoids depending on an indefinitely healthy SSE socket.
      var clientId = Array.from(window.crypto.getRandomValues(new Uint8Array(16)), function (byte) {
        return byte.toString(16).padStart(2, "0");
      }).join("");
      var stopped = false;
      var currentPoll;
      var retryTimer;

      var reply = function (path, nonce, error) {
        return window.fetch(path, {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ nonce: nonce, error: error }),
          })
          .then(function (response) {
            if (!response.ok) console.error("dsh-deck-bridge response HTTP", response.status);
          })
          .catch(function (err) {
            console.error("dsh-deck-bridge response:", err);
          });
      };

      var poll = function () {
        if (stopped) return;
        currentPoll = new window.AbortController();
        window.fetch("/api/deck/next?clientId=" + clientId, {
          cache: "no-store",
          signal: currentPoll.signal,
        })
          .then(function (response) {
            if (!response.ok) throw new Error("poll HTTP " + response.status);
            return response.json();
          })
          .then(function (body) {
            if (stopped || !body.command) return;
            var command = body.command;
            if (!SESSION_ID.test(command.sessionId) || !NONCE.test(command.nonce)) {
              console.error("dsh-deck-bridge invalid command");
              return;
            }
            if (!openSession(command.sessionId)) return reply("/api/deck/nack", command.nonce, lastOpenError);
            return reply("/api/deck/ack", command.nonce);
          })
          .catch(function (err) {
            if (stopped) return;
            console.error("dsh-deck-bridge poll:", err);
            return new Promise(function (resolve) {
              retryTimer = window.setTimeout(resolve, 1000);
            });
          })
          .finally(function () {
            currentPoll = undefined;
            if (!stopped) poll();
          });
      };

      ctx.effect(function () {
        poll();
        return function () {
          stopped = true;
          currentPoll?.abort();
          if (retryTimer !== undefined) window.clearTimeout(retryTimer);
        };
      });
    };

    return module.exports;
  },
});

/* devbox console — browser terminal.
 *
 * xterm.js on one side, a WebSocket to a real pty on the other. Keystrokes go
 * out as binary frames so no byte is reinterpreted on the way; window geometry
 * goes out as a JSON control frame.
 */
(function () {
  "use strict";

  var host = document.getElementById("terminal");
  if (!host) return;

  var statusEl = document.getElementById("term-status");
  function status(text, tone) {
    if (!statusEl) return;
    statusEl.textContent = text;
    statusEl.className = tone ? "muted term-" + tone : "muted";
  }

  if (
    typeof Terminal === "undefined" ||
    typeof FitAddon === "undefined" ||
    typeof FitAddon.FitAddon === "undefined"
  ) {
    status("terminal assets failed to load — reload the page", "err");
    return;
  }

  var term = new Terminal({
    fontFamily:
      'ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, monospace',
    fontSize: 13,
    cursorBlink: true,
    scrollback: 5000,
    theme: prefersDark()
      ? { background: "#070c09", foreground: "#eef7f0", cursor: "#a7ed69" }
      : { background: "#f5f8f3", foreground: "#17221a", cursor: "#2f7f4d" },
  });

  var fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);
  fit.fit();

  var key = window.devboxKey.get();
  var socket = null;
  var encoder = new TextEncoder();
  var stoppedByLifecycle = false;

  // Start first, connect second. Opening both at once made the POST and the
  // WebSocket race for the same lifecycle claim; worse, hx-swap="none" threw
  // away the useful start error and left only WebSocket code 1006 on screen.
  status("starting box…");
  startBox().then(openSocket).catch(function (error) {
    status(error.message || "could not start the box", "err");
    term.options.cursorBlink = false;
  });

  function startBox() {
    var headers = {};
    if (key) headers["X-Devbox-Key"] = key;
    var pendingCard = document.querySelector("#detail-card-slot > .card");
    if (pendingCard) {
      pendingCard.classList.add("htmx-request");
      pendingCard.inert = true;
      pendingCard.setAttribute("aria-busy", "true");
    }
    return fetch(host.dataset.startEndpoint, {
      method: "POST",
      headers: headers,
      credentials: "same-origin",
    }).then(function (response) {
      return response.text().then(function (body) {
        var parsed = new DOMParser().parseFromString(body, "text/html");
        // Read the notice before moving its card into the live document;
        // `replaceWith` adopts that node and removes it from `parsed.body`.
        var notice = parsed.querySelector(".inline-feedback");
        var message = (notice || parsed.body).textContent.trim();
        var freshCard = parsed.querySelector(".card");
        var currentCard = document.querySelector("#detail-card-slot > .card");
        if (freshCard && currentCard) {
          currentCard.replaceWith(freshCard);
          if (window.htmx) window.htmx.process(freshCard);
          document.dispatchEvent(new CustomEvent("devbox:card-updated"));
        } else if (pendingCard && pendingCard.isConnected) {
          pendingCard.classList.remove("htmx-request");
          pendingCard.inert = false;
          pendingCard.removeAttribute("aria-busy");
        }

        if (response.ok) return;
        // Error responses are rendered card fragments. Use their text for the
        // terminal status too, while preserving the card's actionable notice.
        throw new Error(message || "could not start the box (" + response.status + ")");
      });
    }).catch(function (error) {
      if (pendingCard && pendingCard.isConnected) {
        pendingCard.classList.remove("htmx-request");
        pendingCard.inert = false;
        pendingCard.removeAttribute("aria-busy");
      }
      throw error;
    });
  }

  function openSocket() {
    stoppedByLifecycle = false;
    term.options.cursorBlink = true;
    status("connecting…");
    var proto = location.protocol === "https:" ? "wss:" : "ws:";
    var url = proto + "//" + location.host + host.dataset.endpoint;
    // A WebSocket handshake takes no custom headers either, so the key rides
    // the URL as it does for the event stream. It is never navigated to and
    // never becomes a Referer.
    if (key) {
      url +=
        (url.indexOf("?") === -1 ? "?" : "&") +
        "k=" +
        encodeURIComponent(key);
    }
    var sessionSocket = new WebSocket(url);
    socket = sessionSocket;
    sessionSocket.binaryType = "arraybuffer";

    sessionSocket.onopen = function () {
      if (socket !== sessionSocket) return;
      status("connected", "ok");
      sendResize();
      term.focus();
    };

    sessionSocket.onmessage = function (event) {
      if (socket !== sessionSocket) return;
      term.write(new Uint8Array(event.data));
    };

    sessionSocket.onclose = function (event) {
      if (socket !== sessionSocket) return;
      if (stoppedByLifecycle) {
        status("box stopped — start it to open a new session");
        term.options.cursorBlink = false;
        return;
      }
      status(
        event.code === 1000
          ? "session ended — reload to start another"
          : "disconnected (" + event.code + ")",
        event.code === 1000 ? null : "err",
      );
      term.options.cursorBlink = false;
    };

    sessionSocket.onerror = function () {
      if (socket !== sessionSocket) return;
      status("connection failed", "err");
    };
  }

  term.onData(function (data) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(encoder.encode(data));
    }
  });

  // The Stop button lives in the control card above the terminal. Its htmx
  // response replaces only that card, so explicitly reconcile the terminal's
  // connection/status with the same lifecycle result.
  document.addEventListener("devbox:box-status", function (event) {
    if (!event.detail) return;
    if (event.detail.status === "stopped") {
      stoppedByLifecycle = true;
      if (socket && socket.readyState < WebSocket.CLOSING) {
        socket.close(1000, "box stopped");
      }
      status("box stopped — start it to open a new session");
      term.options.cursorBlink = false;
      return;
    }
    if (event.detail.status === "running" && stoppedByLifecycle) {
      // The card's successful Start response is authoritative. Reconnect
      // without another lifecycle POST, so Stop cannot be undone by a late
      // terminal handshake and users do not have to reload the whole page.
      socket = null;
      openSocket();
    }
  });

  function sendResize() {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(
      JSON.stringify({
        type: "resize",
        cols: term.cols,
        rows: term.rows,
      }),
    );
  }

  var resizeTimer = null;
  window.addEventListener("resize", function () {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(function () {
      fit.fit();
      sendResize();
    }, 80);
  });

  function prefersDark() {
    return (
      window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: dark)").matches
    );
  }
})();

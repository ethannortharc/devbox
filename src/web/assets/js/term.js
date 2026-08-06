/* devbox console — browser terminal.
 *
 * xterm.js on one side, a WebSocket to a real pty on the other. Keystrokes go
 * out as binary frames so no byte is reinterpreted on the way; window geometry
 * goes out as a JSON control frame.
 */
(function () {
  "use strict";

  var host = document.getElementById("terminal");
  if (!host || typeof Terminal === "undefined") return;

  var statusEl = document.getElementById("term-status");
  function status(text, tone) {
    if (!statusEl) return;
    statusEl.textContent = text;
    statusEl.className = tone ? "muted term-" + tone : "muted";
  }

  var term = new Terminal({
    fontFamily:
      'ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, monospace',
    fontSize: 13,
    cursorBlink: true,
    scrollback: 5000,
    theme: prefersDark()
      ? { background: "#010409", foreground: "#e6edf3", cursor: "#4493f8" }
      : { background: "#ffffff", foreground: "#1f2328", cursor: "#0969da" },
  });

  var fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);
  fit.fit();

  var proto = location.protocol === "https:" ? "wss:" : "ws:";
  var url = proto + "//" + location.host + host.dataset.endpoint;
  var socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  var encoder = new TextEncoder();

  socket.onopen = function () {
    status("connected", "ok");
    sendResize();
    term.focus();
  };

  socket.onmessage = function (event) {
    term.write(new Uint8Array(event.data));
  };

  socket.onclose = function (event) {
    // 1000 is a clean close: the shell exited.
    status(
      event.code === 1000
        ? "session ended — reload to start another"
        : "disconnected (" + event.code + ")",
      event.code === 1000 ? null : "err",
    );
    term.options.cursorBlink = false;
  };

  socket.onerror = function () {
    status("connection failed", "err");
  };

  term.onData(function (data) {
    if (socket.readyState === WebSocket.OPEN) {
      socket.send(encoder.encode(data));
    }
  });

  function sendResize() {
    if (socket.readyState !== WebSocket.OPEN) return;
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

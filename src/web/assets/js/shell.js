/* devbox console — the shell.
 *
 * A top-level navigation sets no headers, so the first request for any page
 * arrives with no key and is answered with a fixed, data-free document. This is
 * that document's only job: present the key, and replace itself with the real
 * page.
 *
 * It installs a whole document rather than swapping a fragment into this one.
 * The page being installed brings its own <head> — the detail page's terminal
 * tab loads `xterm.js` before `term.js` — and `innerHTML` runs no scripts at
 * all while htmx does not preserve source order for injected `<script src>`.
 * `term.js` returns silently when `Terminal` is undefined, so losing that order
 * yields a terminal tab that never connects and never says why. Handing the
 * browser a document to parse keeps every head and every ordering as it was.
 *
 * ## On `document.write`
 *
 * It is the usual sign of an XSS sink, and it is worth saying plainly why it is
 * not one here. The argument is never attacker-influenced text: it is the
 * console's own response, on its own origin, fetched with the key, produced by
 * the same escaped Askama templates a direct navigation would have rendered.
 * The browser parses the identical bytes either way — this only changes *when*
 * the parse happens, not what is parsed or what it is trusted with.
 *
 * The performance objection does not apply either. That one is about writes
 * interleaved with the initial parse, which block it; this is a single write
 * into a document that has already finished loading.
 *
 * What it buys is the thing the alternatives cannot: a real parse, so `defer`,
 * source order, and `<head>` all behave normally. Reconstructing that by hand —
 * `DOMParser` plus re-injected scripts with `async = false` — is strictly more
 * hand-rolled machinery in the path that decides whether the console works.
 */
(function () {
  "use strict";

  function notice(summary, detail) {
    // Built as nodes, not markup: `detail` is fixed text today, and this is
    // the one file guaranteed to run before the console has authenticated
    // anything. It should not contain an HTML sink at all.
    var main = document.createElement("main");
    main.className = "notice";
    var heading = document.createElement("h1");
    heading.textContent = "devbox";
    var first = document.createElement("p");
    first.textContent = summary;
    var second = document.createElement("p");
    second.textContent = detail;
    main.appendChild(heading);
    main.appendChild(first);
    main.appendChild(second);
    document.body.replaceChildren(main);
  }

  function relaunch(summary) {
    notice(summary, "Re-open the URL printed by `devbox web`.");
  }

  var key = window.devboxKey.get();
  if (!key) {
    relaunch("This tab holds no key for the console on this port.");
    return;
  }

  fetch(location.pathname + location.search, {
    headers: { "X-Devbox-Key": key },
    // Named to say what it means: there is no ambient credential to send, and
    // the console would reject one if there were.
    credentials: "omit",
    // One URL has two answers, told apart by a request header. Without this a
    // cached shell can be replayed to the very fetch trying to replace it.
    cache: "no-store",
  })
    .then(function (res) {
      if (res.status === 401) {
        // Keys are per-launch. A stored one that stopped working means the
        // console was restarted, so drop it — otherwise every later reload
        // fails the same way, and re-opening the printed URL is the only cure
        // for a state the page never admits it is in.
        window.devboxKey.clear();
        relaunch("The stored key has expired — the console was restarted.");
        return null;
      }
      return res.text();
    })
    .then(function (html) {
      if (html === null) {
        return;
      }
      document.open();
      document.write(html);
      document.close();
    })
    .catch(function () {
      notice(
        "Could not reach the console.",
        "It may have stopped. Check the terminal running `devbox web`."
      );
    });
})();

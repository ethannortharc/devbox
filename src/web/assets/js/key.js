/* devbox console — the console key, and the only place it is touched.
 *
 * The key is what every request carrying data is judged by. It lives in
 * `localStorage`, which is scoped to a whole origin — scheme, host *and port*
 * — so a service on another 127.0.0.1 port cannot read it. That is the entire
 * reason it is not a cookie: cookies are scoped by host alone, so the console's
 * used to be handed to every other loopback service the browser spoke to.
 *
 * Nothing attaches this automatically. Each caller below sends it on purpose,
 * which is also why the console no longer has a CSRF surface: forgery rides
 * ambient credentials, and there is no longer an ambient credential.
 *
 * Four callers need it — the shell, the bootstrap page, every htmx request, and
 * the terminal socket — so the storage name is defined once, here. A second
 * spelling of it somewhere else would be a console that authenticates on three
 * paths out of four.
 */
window.devboxKey = (function () {
  "use strict";

  var NAME = "devbox.key";

  // Storage access *throws* rather than returning null when a browser has it
  // disabled, or in some private-browsing modes. An uncaught exception here
  // would leave a blank page and no clue, so every access is guarded and the
  // callers decide what to say about it.
  return {
    get: function () {
      try {
        return window.localStorage.getItem(NAME);
      } catch (e) {
        return null;
      }
    },
    set: function (value) {
      try {
        window.localStorage.setItem(NAME, value);
        return true;
      } catch (e) {
        return false;
      }
    },
    clear: function () {
      try {
        window.localStorage.removeItem(NAME);
      } catch (e) {
        /* nothing to clear if storage is unavailable */
      }
    },
  };
})();

/* devbox console — the console key, and the only place it is touched.
 *
 * The key is what every request carrying data is judged by. It lives in
 * `localStorage` on a fresh random `devbox-….localhost` hostname created for
 * each console launch. Storage is scoped to an origin — scheme, host *and
 * port* — so all tabs on the current launch can read it, while another
 * loopback service, the stable 127.0.0.1 console address, and every previous
 * launch cannot. That is why it is not a cookie: cookies are scoped by host
 * alone, so the console's used to be handed to every other loopback service
 * the browser spoke to.
 *
 * ## Why local storage is safe here
 *
 * Same origin is not the same program, so this was unsafe while the browser
 * origin was the predictable `127.0.0.1:7878`. The server now separates its
 * stable listening address from its browser origin: each launch prints a new
 * random `.localhost` hostname and redirects bare loopback navigations there.
 * An old page does not know that unguessable origin and cannot observe its
 * storage. Sharing within it is precisely what makes independently opened tabs
 * reentrant.
 *
 * Nothing attaches this automatically. Each caller below sends it on purpose,
 * which is also why the console has no CSRF surface: forgery rides ambient
 * credentials, and there is no longer an ambient credential.
 *
 * Five callers need it — the shell, the bootstrap page, every htmx request, the
 * terminal socket, and keyed downloads — so the storage name is defined once,
 * here. A second spelling of it elsewhere would be a console that authenticates
 * on four paths out of five.
 */
window.devboxKey = (function () {
  "use strict";

  var NAME = "devbox.key";

  // Sweep up the key the preceding per-tab build wrote. On the random
  // per-launch origin it can only be stale or a duplicate; keeping one source
  // of truth makes upgrade behaviour deterministic.
  try {
    window.sessionStorage.removeItem(NAME);
  } catch (e) {
    /* nothing to sweep if storage is unavailable */
  }

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

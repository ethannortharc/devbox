/* devbox console — the console key, and the only place it is touched.
 *
 * The key is what every request carrying data is judged by. It lives in
 * `sessionStorage`, which is scoped to an origin — scheme, host *and port* — so
 * a service on another 127.0.0.1 port cannot read it. That is why it is not a
 * cookie: cookies are scoped by host alone, so the console's used to be handed
 * to every other loopback service the browser spoke to.
 *
 * ## Why session and not local
 *
 * Same origin is not the same program. The console binds a predictable port,
 * and a page served earlier from that port by something else — a project's own
 * dev server, since stopped — shares this origin exactly. `localStorage` is
 * shared by every tab on an origin *and* announces writes to them through the
 * `storage` event, so such a page, still open, would be handed the key the
 * moment the console installed it, and could replay it same-origin against the
 * terminal and lifecycle routes.
 *
 * `sessionStorage` is per tab. Another tab cannot read this one's, and no
 * cross-tab event fires. It is copied into tabs opened from the console, and
 * restored by session restore, so it costs less than it sounds: the key is
 * per-launch anyway, which already meant no bookmark outlived a restart. What
 * it does cost is a freshly typed URL or bookmark during a live launch, which
 * gets a notice telling the user to re-open the printed URL.
 *
 * One residual, recorded rather than hidden: within a *single* tab, history or
 * bfcache could restore that earlier page into a tab whose sessionStorage now
 * holds the key. Nothing available to a page on a fixed loopback origin closes
 * that; see ADR-0048.
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

  // Sweep up the key this used to write. Keys are per-launch, so anything left
  // in `localStorage` by an older build is already dead — but it is
  // credential-shaped and it sits in precisely the storage that other tabs on
  // this origin can read, which is why it stopped living there. Leaving it is
  // leaving litter in the place the finding was about.
  try {
    window.localStorage.removeItem(NAME);
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
        return window.sessionStorage.getItem(NAME);
      } catch (e) {
        return null;
      }
    },
    set: function (value) {
      try {
        window.sessionStorage.setItem(NAME, value);
        return true;
      } catch (e) {
        return false;
      }
    },
    clear: function () {
      try {
        window.sessionStorage.removeItem(NAME);
      } catch (e) {
        /* nothing to clear if storage is unavailable */
      }
    },
  };
})();

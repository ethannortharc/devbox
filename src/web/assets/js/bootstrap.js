/* devbox console — key installation.
 *
 * Reached once, by the URL `devbox web` prints. The token in that URL buys
 * exactly this page and nothing else; the key this page installs is what every
 * later request is judged by.
 */
(function () {
  "use strict";

  function meta(name) {
    var el = document.querySelector('meta[name="' + name + '"]');
    return el ? el.getAttribute("content") : null;
  }

  var key = meta("devbox-key");
  var target = meta("devbox-target") || "/";

  if (!key || !window.devboxKey.set(key)) {
    document.body.textContent =
      "devbox: this browser will not let the console store its key. " +
      "The console needs localStorage on this origin.";
    return;
  }

  // `replace`, not `assign`: the URL carrying the token must not be left as a
  // history entry the back button can return to.
  location.replace(target);
})();

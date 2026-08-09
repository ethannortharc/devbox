/* devbox console — keyed downloads.
 *
 * An `<a href>` is a navigation, and a navigation cannot present the console
 * key. The behaviour-export links therefore started returning 401 the moment
 * the credential stopped being a cookie: `/api/` is not eligible for the shell,
 * so there was no first hop to fall back to and the user got a refusal instead
 * of a file.
 *
 * Putting the key in the `href` would have worked, and is the one thing the
 * design refuses. `?k=` is accepted under `/api/` for the two channels that
 * cannot set a header, and a clicked link is not one of them — that URL would
 * land in history, in the downloads list, and on the clipboard of anyone who
 * used "Copy link address". Fetch it with the key and hand the browser a blob
 * instead, which also makes these real downloads rather than a page the browser
 * renders however it feels about the content type.
 */
(function () {
  "use strict";

  document.addEventListener("click", function (e) {
    var link = e.target.closest && e.target.closest("a[data-download]");
    if (!link) {
      return;
    }
    // Leave the browser's own gestures alone: middle-click, cmd/ctrl-click and
    // "open in new tab" are the user asking for a navigation, and a navigation
    // is exactly what cannot carry the key. Better it fails visibly as a 401
    // than that this quietly swallows the gesture and does something else.
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) {
      return;
    }

    var key = window.devboxKey.get();
    if (!key) {
      return; // let it navigate; the refusal says what to do
    }
    e.preventDefault();

    var filename = link.getAttribute("data-download") || "devbox-export";
    fetch(link.href, {
      headers: { "X-Devbox-Key": key },
      credentials: "omit",
      cache: "no-store",
    })
      .then(function (res) {
        if (!res.ok) {
          throw new Error("HTTP " + res.status);
        }
        return res.blob();
      })
      .then(function (blob) {
        var url = URL.createObjectURL(blob);
        var tmp = document.createElement("a");
        tmp.href = url;
        tmp.download = filename;
        document.body.appendChild(tmp);
        tmp.click();
        tmp.remove();
        // Not revoked synchronously: some browsers have not finished reading
        // the blob by the time `click()` returns, and revoking early produces
        // an empty file rather than an error.
        setTimeout(function () {
          URL.revokeObjectURL(url);
        }, 0);
      })
      .catch(function (err) {
        // Say so in the page. A download that silently does nothing is the
        // failure mode people retry five times before reporting.
        var note = link.parentNode.querySelector(".export-error");
        if (!note) {
          note = document.createElement("span");
          note.className = "muted export-error";
          link.parentNode.appendChild(note);
        }
        note.textContent = " — export failed (" + err.message + ")";
      });
  });
})();

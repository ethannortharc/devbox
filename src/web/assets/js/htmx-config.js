// htmx response handling for devbox.
//
// htmx 2 ships `{code:"[45]..", swap:false, error:true}`, so a 4xx response
// body is never swapped into the page. Every actionable error this console
// produces — "that box has uncommitted overlay changes", "that selection is
// invalid", "a rebuild is already running" — is returned as a fragment with a
// 4xx status, and none of them reached the user. The request looked like it
// had silently done nothing.
//
// Returning 200 for errors would have hidden the failure from anything reading
// the status: the tests, `curl`, a future script. So the status stays honest
// and htmx is told to render the body.
//
// Set directly rather than from an event: htmx exposes no configuration event
// (`htmx:configRequest` fires per request, far too late), and both scripts are
// `defer`, which guarantees this runs after htmx.min.js and before any request.
htmx.config.responseHandling = [
  { code: '204', swap: false },
  { code: '[23]..', swap: true },
  // 4xx carries an explanation the user needs to see. Still marked as an error
  // so htmx fires htmx:responseError for anything listening.
  { code: '4..', swap: true, error: true },
  // 5xx is a bug rather than a message; leave the page as it was.
  { code: '5..', swap: false, error: true },
];

// Every htmx request presents the console key.
//
// Nothing sends it for us — that is the property the console is built on, not
// an inconvenience. A cookie would ride along automatically and would also ride
// along to every other service on 127.0.0.1, which is exactly how the token
// used to leak. `htmx:configRequest` is the documented hook for this, fires
// before the request is issued, and bubbles to `document`.
document.addEventListener('htmx:configRequest', function (e) {
  var key = window.devboxKey.get();
  if (key) {
    e.detail.headers['X-Devbox-Key'] = key;
  }
});

// `EventSource` accepts no headers, so the stream carries its key in the URL.
//
// Installed here rather than left to the SSE extension: that extension claims
// `htmx.createEventSource` only if it is still undefined, and this file is
// loaded before it. A stream opened without the key would 401 and leave the
// heartbeat indicator grey with the rest of the page working.
htmx.createEventSource = function (url) {
  var key = window.devboxKey.get();
  if (key) {
    url += (url.indexOf('?') === -1 ? '?' : '&') + 'k=' + encodeURIComponent(key);
  }
  return new EventSource(url);
};

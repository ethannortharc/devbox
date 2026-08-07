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

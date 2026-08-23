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
  // 5xx responses are escaped, designed error fragments. Showing them is
  // essential: otherwise a server failure looks exactly like a dead button.
  { code: '5..', swap: true, error: true },
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

// Apply every live grid snapshot card by card. Replacing the whole grid would
// detach an active request target, while doing so immediately after the request
// would erase its actionable error response. Per-card merging protects active
// cards and carries same-state errors while every other box keeps updating.
function mergeBoxesSnapshot(html) {
  var grid = document.getElementById('box-grid');
  if (!grid) return;

  var template = document.createElement('template');
  template.innerHTML = html;
  var incoming = new Map();
  template.content.querySelectorAll('.card[data-box-name]').forEach(function (card) {
    incoming.set(card.dataset.boxName, card);
  });

  var installed = [];
  grid.querySelectorAll('.card[data-box-name]').forEach(function (current) {
    var name = current.dataset.boxName;
    // htmx adds the request class to the resolved indicator itself. Lifecycle
    // buttons use `hx-indicator="closest .card"`, so the class normally lives
    // on `current`; keep the descendant check for future nested indicators.
    if (current.matches('.htmx-request') || current.querySelector('.htmx-request')) {
      incoming.delete(name);
      return;
    }

    var replacement = incoming.get(name);
    if (replacement) {
      incoming.delete(name);
      preserveLifecycleErrorForSameStatus(current, replacement);
      current.replaceWith(replacement);
      installed.push(replacement);
    } else {
      current.remove();
    }
  });

  incoming.forEach(function (card) {
    grid.appendChild(card);
    installed.push(card);
  });

  if (!grid.querySelector('.card[data-box-name]')) {
    grid.innerHTML = html;
    htmx.process(grid);
    return;
  }
  grid.querySelectorAll('.empty').forEach(function (empty) {
    empty.remove();
  });
  installed.forEach(function (card) {
    htmx.process(card);
  });
}

// Carry a lifecycle error only across a snapshot that confirms the same state.
// This protects a fail-closed Start response from the next notice-free stopped
// snapshot, while a real transition from another tab or the CLI clears the now
// stale error. The server renders this status marker on every card.
function preserveLifecycleErrorForSameStatus(current, incoming) {
  var error = current && current.querySelector('.inline-feedback.error');
  if (
    !error ||
    !current.dataset.boxStatus ||
    current.dataset.boxStatus !== incoming.dataset.boxStatus ||
    incoming.querySelector('.inline-feedback.error')
  ) {
    return false;
  }
  incoming.appendChild(error.cloneNode(true));
  return true;
}

// Keep an actionable lifecycle error while still accepting newer same-state
// data for the detail card. A Start that fails closed commonly produces the
// exact stopped state the watcher publishes next; replacing the response with
// that notice-free snapshot made the reason disappear a few seconds later.
function mergeDetailSnapshotPreservingError(html) {
  var slot = document.getElementById('detail-card-slot');
  var current = slot && slot.querySelector(':scope > .card');
  if (!current || !current.querySelector('.inline-feedback.error')) return false;

  var template = document.createElement('template');
  template.innerHTML = html;
  var incoming = template.content.querySelector('.card');
  if (!incoming) return false;
  if (!preserveLifecycleErrorForSameStatus(current, incoming)) return false;
  current.replaceWith(incoming);
  htmx.process(incoming);
  document.dispatchEvent(new CustomEvent('devbox:card-updated'));
  return true;
}

// Do not let a live status update replace a lifecycle request's target.
//
// Start can legitimately spend up to the VM readiness deadline waiting for a
// guest shell. During that window the watcher sees `stopped -> unreachable`
// and publishes a fresh grid. If it replaces the card carrying the request,
// htmx later swaps the final success/error response into a detached node and
// the user never sees it. Keeping the current grid for the duration also keeps
// the operation's buttons disabled, so Stop cannot race an in-flight Start.
// This script runs in <head>, before `document.body` exists. The event bubbles
// from the swap target, so the document is both early-safe and sufficient.
document.addEventListener('htmx:sseBeforeMessage', function (e) {
  var type = e.detail.type || '';
  var removesBox =
    type.indexOf('box-card-') === 0 &&
    (e.detail.data || '').indexOf('data-box-removed="true"') !== -1;

  // A retained-status GET starts as soon as its panel is installed. If a
  // newer completion SSE wins that race, remember it so the older HTTP
  // snapshot cannot arrive afterwards and overwrite the final result.
  if (type.indexOf('build-status-') === 0 && e.target.dataset) {
    e.target.dataset.sseSeen = 'true';
  }

  if (type === 'boxes' && document.getElementById('box-grid')) {
    mergeBoxesSnapshot(e.detail.data || '');
    e.preventDefault();
    return;
  }

  if (
    type.indexOf('box-card-') === 0 &&
    !removesBox &&
    document.querySelector('#detail-card-slot .htmx-request')
  ) {
    e.preventDefault();
    return;
  }

  if (
    type.indexOf('box-card-') === 0 &&
    !removesBox &&
    mergeDetailSnapshotPreservingError(e.detail.data || '')
  ) {
    e.preventDefault();
  }
});

document.addEventListener('htmx:beforeSwap', function (e) {
  var target = e.detail.target;
  var xhr = e.detail.xhr;
  if (
    target &&
    target.dataset &&
    target.dataset.buildStatus &&
    target.dataset.sseSeen === 'true' &&
    xhr &&
    xhr.responseURL.indexOf('/api/operations/') !== -1
  ) {
    e.detail.shouldSwap = false;
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

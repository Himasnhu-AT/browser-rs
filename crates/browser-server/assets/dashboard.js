/* The operator dashboard's client.
 *
 * Two modes, chosen by the query string:
 *
 *   /?token=…               the index: the session table and the activity log
 *   /?session=NAME&token=…  the live view of one session
 *
 * Both open the same socket. `/ws` with no session streams the activity log
 * only; `/ws?session=NAME` adds the frames and accepts input back.
 *
 * The token rides in the query string because a browser cannot put an
 * `Authorization` header on a top-level navigation or on a `WebSocket`
 * constructor. See `ws::auth` for the trade that was made there.
 *
 * No framework, no build step, no dependency. This file is compiled into the
 * server binary. */

(function () {
  'use strict';

  var params = new URLSearchParams(location.search);
  var TOKEN = params.get('token');
  var SESSION = params.get('session');
  var POLL_MS = 3000;
  var LOG_MAX = 200;

  var $ = function (id) { return document.getElementById(id); };

  var conn = $('conn');
  var log = $('log');
  var noActivity = $('no-activity');

  /* ------------------------------------------------------------------ */
  /* URLs                                                                */
  /* ------------------------------------------------------------------ */

  function withToken(path, extra) {
    var url = new URL(path, location.href);
    if (TOKEN) url.searchParams.set('token', TOKEN);
    if (extra) Object.keys(extra).forEach(function (k) {
      url.searchParams.set(k, extra[k]);
    });
    return url;
  }

  function socketUrl() {
    var url = withToken('/ws', SESSION ? { session: SESSION } : null);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    return url.toString();
  }

  $('home').href = withToken('/').toString();

  /* ------------------------------------------------------------------ */
  /* The activity log                                                     */
  /* ------------------------------------------------------------------ */

  /* Last action per session, so the index table can show it. */
  var lastAction = Object.create(null);

  function clockOf(iso) {
    /* The engine sends RFC 3339 UTC. Only the wall clock is interesting in a
     * log the operator is watching scroll past. */
    var at = new Date(iso);
    return isNaN(at.getTime()) ? iso : at.toLocaleTimeString();
  }

  function appendLog(at, kind, detail, isError) {
    noActivity.hidden = true;
    var li = document.createElement('li');
    if (isError) li.className = 'error';

    var time = document.createElement('time');
    time.textContent = clockOf(at);
    var name = document.createElement('span');
    name.className = 'kind';
    name.textContent = kind;
    var body = document.createElement('span');
    body.className = 'detail';
    /* textContent, never innerHTML: `detail` is a URL or a selector that came
     * from whoever called the API. */
    body.textContent = detail;

    li.appendChild(time);
    li.appendChild(name);
    li.appendChild(body);
    log.appendChild(li);
    while (log.children.length > LOG_MAX) log.removeChild(log.firstChild);
    log.scrollTop = log.scrollHeight;
  }

  /* ------------------------------------------------------------------ */
  /* The index                                                            */
  /* ------------------------------------------------------------------ */

  function duration(seconds) {
    if (seconds < 60) return seconds + 's';
    if (seconds < 3600) return Math.floor(seconds / 60) + 'm';
    if (seconds < 86400) return Math.floor(seconds / 3600) + 'h';
    return Math.floor(seconds / 86400) + 'd';
  }

  function renderSessions(overview) {
    $('no-chrome').hidden = !!overview.chrome;

    var facts = [];
    facts.push(overview.chrome ? 'Chromium: ' + overview.chrome : 'no Chromium found');
    facts.push(overview.headless ? 'headless' : 'headful');
    facts.push('up to ' + overview.max_live_browsers + ' live');
    if (!overview.authenticated) facts.push('no token — loopback only');
    $('server-facts').textContent = facts.join(' · ');

    var rows = $('session-rows');
    rows.textContent = '';
    $('no-sessions').hidden = overview.sessions.length > 0;

    overview.sessions.forEach(function (session) {
      var tr = document.createElement('tr');

      var nameCell = document.createElement('td');
      var link = document.createElement('a');
      link.className = 'session';
      link.href = withToken('/', { session: session.name }).toString();
      link.textContent = session.name;
      nameCell.appendChild(link);
      var recent = lastAction[session.name];
      if (recent) {
        var note = document.createElement('div');
        note.className = 'fine';
        note.textContent = recent;
        nameCell.appendChild(note);
      }

      var urlCell = document.createElement('td');
      var url = document.createElement('span');
      url.className = 'url';
      url.textContent = session.url || '—';
      if (session.url) url.title = session.url;
      urlCell.appendChild(url);

      var browserCell = document.createElement('td');
      var dot = document.createElement('span');
      dot.className = 'dot';
      dot.dataset.live = String(session.browser);
      dot.textContent = session.browser ? '● live' : '○ none';
      browserCell.appendChild(dot);

      [nameCell, urlCell, browserCell].forEach(function (cell) { tr.appendChild(cell); });
      [session.age_secs, session.idle_secs, session.requests].forEach(function (value, index) {
        var cell = document.createElement('td');
        cell.className = 'num';
        cell.textContent = index === 2 ? String(value) : duration(value);
        tr.appendChild(cell);
      });
      rows.appendChild(tr);
    });
  }

  function pollSessions() {
    fetch(withToken('/dashboard/sessions.json').toString(), { cache: 'no-store' })
      .then(function (response) {
        if (!response.ok) throw new Error('HTTP ' + response.status);
        return response.json();
      })
      .then(renderSessions)
      .catch(function (error) {
        appendLog(new Date().toISOString(), 'dashboard', String(error), true);
      });
  }

  /* ------------------------------------------------------------------ */
  /* The live view                                                        */
  /* ------------------------------------------------------------------ */

  var frame = $('frame');
  var stage = $('stage');
  var controlButton = $('control');
  var controlling = false;
  /* The frame's own pixel size, which is the space the server wants
   * coordinates in. Not the size the <img> is drawn at, and not the page's. */
  var frameW = 0;
  var frameH = 0;
  var socket = null;

  var BUTTONS = ['left', 'middle', 'right', 'back', 'forward'];
  /* Keys that type something but whose `key` is longer than one character. */
  var CHAR_TEXT = { Enter: '\r', Tab: '\t' };

  function send(message) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(message));
    }
  }

  /* CDP's modifier bit field: Alt=1, Ctrl=2, Meta=4, Shift=8. */
  function modifiers(event) {
    return (event.altKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) |
           (event.metaKey ? 4 : 0) | (event.shiftKey ? 8 : 0);
  }

  /* From the pixel the operator clicked to the pixel of the frame image.
   * The server takes it from there into the page's own coordinates — it is the
   * only side that knows the device scale factor. */
  function imagePoint(event) {
    var box = frame.getBoundingClientRect();
    if (!box.width || !box.height) return { x: 0, y: 0 };
    return {
      x: (event.clientX - box.left) * (frameW / box.width),
      y: (event.clientY - box.top) * (frameH / box.height)
    };
  }

  function setControlling(next) {
    controlling = next;
    send({ t: 'control', take: next });
    controlButton.setAttribute('aria-pressed', String(next));
    controlButton.textContent = next ? 'Release control' : 'Take control';
    stage.dataset.controlling = String(next);
    $('stage-badge').hidden = !next;
    var hint = $('hint');
    hint.dataset.controlling = String(next);
    hint.textContent = next
      ? 'You have the controls. Type the login by hand — the session keeps it. ' +
        'Release when you are done and the agent carries on, signed in.'
      : 'Watching. Your clicks and keystrokes are not being sent.';
  }

  function bindStage() {
    controlButton.addEventListener('click', function () {
      setControlling(!controlling);
    });

    ['mousedown', 'mouseup', 'mousemove'].forEach(function (type) {
      frame.addEventListener(type, function (event) {
        if (!controlling) return;
        event.preventDefault();
        var point = imagePoint(event);
        send({
          t: 'mouse',
          kind: type === 'mousedown' ? 'down' : type === 'mouseup' ? 'up' : 'move',
          x: point.x,
          y: point.y,
          button: BUTTONS[event.button] || 'none',
          clicks: event.detail || 1,
          modifiers: modifiers(event)
        });
      });
    });

    /* The page under the frame is the thing that should scroll, not ours. */
    frame.addEventListener('wheel', function (event) {
      if (!controlling) return;
      event.preventDefault();
      var point = imagePoint(event);
      send({
        t: 'mouse', kind: 'wheel', x: point.x, y: point.y,
        dx: event.deltaX, dy: event.deltaY, modifiers: modifiers(event)
      });
    }, { passive: false });

    /* A right-click belongs to the remote page. */
    frame.addEventListener('contextmenu', function (event) {
      if (controlling) event.preventDefault();
    });

    document.addEventListener('keydown', function (event) {
      if (!controlling) return;
      /* A held Ctrl/Cmd is a shortcut the operator means for *their* browser as
       * often as for the page — copy, paste, the address bar. Forward it, but
       * do not swallow it. */
      var shortcut = event.ctrlKey || event.metaKey;
      if (!shortcut) event.preventDefault();

      send({
        t: 'key', kind: 'down', key: event.key, code: event.code,
        modifiers: modifiers(event)
      });

      /* The insertion is a separate `char` event, because that is the only CDP
       * event that types. A `keyDown` carrying text as well would double every
       * character. */
      if (!shortcut) {
        var text = event.key.length === 1 ? event.key : CHAR_TEXT[event.key];
        if (text) {
          send({ t: 'key', kind: 'char', text: text, key: event.key, code: event.code });
        }
      }
    });

    document.addEventListener('keyup', function (event) {
      if (!controlling) return;
      send({ t: 'key', kind: 'up', key: event.key, code: event.code, modifiers: modifiers(event) });
    });

    /* Paste is why a password manager works here at all: a synthetic Ctrl+V
     * does not reach the remote page's clipboard, so the text is sent instead. */
    document.addEventListener('paste', function (event) {
      if (!controlling) return;
      var text = event.clipboardData && event.clipboardData.getData('text');
      if (!text) return;
      event.preventDefault();
      send({ t: 'text', text: text });
    });
  }

  function showFrame(message) {
    frameW = message.w;
    frameH = message.h;
    frame.src = 'data:image/' + message.format + ';base64,' + message.data;
    $('stage-empty').hidden = true;
  }

  function showStatus(message) {
    if (!SESSION) return;
    $('live-url').textContent = message.url || 'no page yet';
    if (message.url) $('live-url').title = message.url;
    controlButton.disabled = !message.browser;
    if (!message.browser) {
      /* The browser went away, or never arrived. Taking control of nothing is
       * worse than being told there is nothing to control. */
      frame.removeAttribute('src');
      $('stage-empty').hidden = false;
      $('stage-message').textContent = 'No browser attached to this session.';
      if (controlling) setControlling(false);
    } else if (!frame.getAttribute('src')) {
      $('stage-message').textContent = 'Browser attached. Waiting for a frame…';
    }
  }

  /* ------------------------------------------------------------------ */
  /* The socket                                                           */
  /* ------------------------------------------------------------------ */

  var retryMs = 500;

  function connect() {
    socket = new WebSocket(socketUrl());

    socket.onopen = function () {
      retryMs = 500;
      conn.textContent = 'live';
      conn.dataset.state = 'on';
      /* The server starts every socket watching. Say so explicitly rather than
       * relying on both sides having the same default. */
      if (SESSION) setControlling(false);
    };

    socket.onmessage = function (event) {
      var message;
      try {
        message = JSON.parse(event.data);
      } catch (error) {
        return;
      }
      if (message.t === 'frame') showFrame(message);
      else if (message.t === 'status') showStatus(message);
      else if (message.t === 'action') {
        appendLog(message.at, message.kind, message.detail, false);
        /* `session` is present only when the action ran on one. It is what
         * lets the index table say what each session last did. */
        if (message.session) {
          lastAction[message.session] = message.kind + ' · ' + message.detail;
        }
      }
      else if (message.t === 'error') {
        appendLog(new Date().toISOString(), 'error', message.message, true);
        if (SESSION && !frame.getAttribute('src')) {
          $('stage-message').textContent = message.message;
          $('stage-empty').hidden = false;
        }
      }
    };

    socket.onclose = function () {
      conn.textContent = 'reconnecting…';
      conn.dataset.state = 'off';
      if (controlling) setControlling(false);
      /* Backing off keeps a server that is down from being hammered, and keeps
       * a dashboard left open overnight from filling a log with failures. */
      setTimeout(connect, retryMs);
      retryMs = Math.min(retryMs * 2, 10000);
    };

    socket.onerror = function () { /* `onclose` follows, and handles it. */ };
  }

  /* ------------------------------------------------------------------ */
  /* Start                                                                */
  /* ------------------------------------------------------------------ */

  if (SESSION) {
    $('index-view').hidden = true;
    $('live-view').hidden = false;
    $('live-name').textContent = SESSION;
    $('crumb').textContent = SESSION;
    document.title = SESSION + ' — browser-rs';
    bindStage();
  } else {
    pollSessions();
    setInterval(pollSessions, POLL_MS);
  }

  connect();
})();

// Runs from <head>: the wrap choice is applied before first paint.
(function () {
  var root = document.documentElement;
  try {
    if (localStorage.getItem('margin.wrap') === 'off') root.classList.add('nowrap');
  } catch (e) {}
  document.addEventListener('DOMContentLoaded', function () {
    var button = document.getElementById('wrap-toggle');
    if (!button) return;
    function sync() {
      var off = root.classList.contains('nowrap');
      button.textContent = off ? 'Wrap: off' : 'Wrap: on';
      button.setAttribute('aria-pressed', off ? 'false' : 'true');
    }
    sync();
    button.addEventListener('click', function () {
      var off = root.classList.toggle('nowrap');
      try {
        localStorage.setItem('margin.wrap', off ? 'off' : 'on');
      } catch (e) {}
      sync();
    });
  });

  document.addEventListener('DOMContentLoaded', comments);

  // Comments: tap or hold-and-drag a line number, a mark opens its thread. Pointer Events only.
  function comments() {
    var HOLD_MS = 400; // a press this long without moving starts a range
    var SLOP = 10; // px a finger may wander and still be holding or tapping
    var EDGE = 48; // px from the viewport's top or bottom where dragging scrolls the page
    var MAX_SPEED = 28; // px per frame at the very edge
    var collection = document.body.getAttribute('data-collection');
    var body = document.body;
    var g = null; // the gesture in flight
    var box = null; // the open new-comment form

    function rowsOf(lines) {
      if (!lines._rows) lines._rows = Array.prototype.slice.call(lines.querySelectorAll('[data-line]'));
      return lines._rows;
    }

    function lineAt(lines, x, y) {
      var el = document.elementFromPoint(x, y);
      var row = el && el.closest('[data-line]');
      return row && lines.contains(row) ? Number(row.getAttribute('data-line')) : null;
    }

    function paint(lines, a, b, on) {
      var rows = rowsOf(lines);
      for (var n = Math.min(a, b); n <= Math.max(a, b); n++) rows[n - 1].classList.toggle('sel', on);
    }

    function closeBox() {
      if (box) box.remove();
      box = null;
    }

    function openBox(lines, first, last) {
      closeBox();
      var form = document.createElement('form');
      form.className = 'newbox';
      form.setAttribute('data-first', first);
      form.setAttribute('data-last', last);
      form.setAttribute('data-version', lines.getAttribute('data-version'));
      var who = document.createElement('div');
      who.className = 'who';
      who.textContent = first === last ? 'Line ' + first : 'Lines ' + first + '\u2013' + last;
      var area = document.createElement('textarea');
      area.name = 'body';
      area.rows = 3;
      area.setAttribute('aria-label', 'Comment');
      var post = document.createElement('button');
      post.type = 'submit';
      post.textContent = 'Post';
      var cancel = document.createElement('button');
      cancel.type = 'button';
      cancel.setAttribute('data-act', 'cancel');
      cancel.textContent = 'Cancel';
      form.append(who, area, post, cancel);
      rowsOf(lines)[last - 1].after(form);
      box = form;
      area.focus({ preventScroll: true });
    }

    function stopGesture() {
      if (!g) return;
      clearTimeout(g.timer);
      if (g.ranging) paint(g.lines, g.start, g.cur, false);
      body.removeAttribute('data-gesture');
      g.ranging = false;
      g = null;
    }

    function setCur(n) {
      if (n === null || n === g.cur) return;
      paint(g.lines, g.start, g.cur, false);
      g.cur = n;
      paint(g.lines, g.start, g.cur, true);
    }

    function beginRange() {
      g.ranging = true;
      g.cur = g.start;
      paint(g.lines, g.start, g.cur, true);
      body.setAttribute('data-gesture', 'ranging');
      if (navigator.vibrate) navigator.vibrate(10);
      requestAnimationFrame(tick);
    }

    // While ranging: scroll near the edges, and keep the range on the line under the finger even
    // when the finger is still and the page is moving under it.
    function tick() {
      if (!g || !g.ranging) return;
      var h = window.innerHeight;
      var dy = 0;
      if (g.y < EDGE) dy = -MAX_SPEED * Math.min(1, (EDGE - g.y) / EDGE);
      else if (g.y > h - EDGE) dy = MAX_SPEED * Math.min(1, (g.y - (h - EDGE)) / EDGE);
      if (dy) window.scrollBy(0, dy);
      setCur(lineAt(g.lines, g.x, g.y));
      requestAnimationFrame(tick);
    }

    document.addEventListener('pointerdown', function (e) {
      var no = e.target.closest && e.target.closest('.no');
      if (!no || g || (e.pointerType === 'mouse' && e.button !== 0)) return;
      var row = no.closest('[data-line]');
      g = {
        id: e.pointerId,
        lines: row.parentNode,
        start: Number(row.getAttribute('data-line')),
        cur: Number(row.getAttribute('data-line')),
        x0: e.clientX,
        y0: e.clientY,
        x: e.clientX,
        y: e.clientY,
        ranging: false,
        timer: setTimeout(beginRange, HOLD_MS),
      };
    });

    document.addEventListener('pointermove', function (e) {
      if (!g || e.pointerId !== g.id) return;
      g.x = e.clientX;
      g.y = e.clientY;
      if (g.ranging) {
        setCur(lineAt(g.lines, g.x, g.y));
      } else if (Math.abs(g.x - g.x0) > SLOP || Math.abs(g.y - g.y0) > SLOP) {
        stopGesture(); // moved before the hold: a swipe, the browser scrolls
      }
    });

    // Once the hold has begun the finger must not scroll the page; the first touchmove after the
    // hold is still cancelable because the browser has not started a scroll.
    document.addEventListener(
      'touchmove',
      function (e) {
        if (g && g.ranging && e.cancelable) e.preventDefault();
      },
      { passive: false }
    );

    document.addEventListener('pointerup', function (e) {
      if (!g || e.pointerId !== g.id) return;
      var lines = g.lines;
      var first = Math.min(g.start, g.cur);
      var last = Math.max(g.start, g.cur);
      if (g.ranging) {
        var at = lineAt(lines, e.clientX, e.clientY);
        if (at !== null) {
          first = Math.min(g.start, at);
          last = Math.max(g.start, at);
        }
      }
      var done = g.ranging || Math.abs(e.clientX - g.x0) <= SLOP && Math.abs(e.clientY - g.y0) <= SLOP;
      stopGesture();
      if (done) openBox(lines, first, last);
    });

    document.addEventListener('pointercancel', function (e) {
      if (g && e.pointerId === g.id) stopGesture();
    });

    document.addEventListener('contextmenu', function (e) {
      if (e.target.closest && e.target.closest('.no')) e.preventDefault();
    });

    function send(method, url, payload) {
      return fetch(url, {
        method: method,
        headers: { 'content-type': 'application/json' },
        body: payload === undefined ? undefined : JSON.stringify(payload),
      });
    }

    function swap(old, html) {
      var t = document.createElement('template');
      t.innerHTML = html.trim();
      var fresh = t.content.firstElementChild;
      old.replaceWith(fresh);
      return fresh;
    }

    // Redraw every mark of one document from the server's answer.
    function drawMarks(lines, res) {
      var header = res.headers.get('x-margin-marks');
      if (!header) return;
      lines.querySelectorAll('.mark').forEach(function (m) {
        m.remove();
      });
      var rows = rowsOf(lines);
      JSON.parse(header).forEach(function (m) {
        for (var n = m.first; n <= m.last; n++) {
          var mark = document.createElement('span');
          mark.className = 'mark';
          mark.setAttribute('data-thread', m.thread);
          mark.style.setProperty('--lane', m.lane);
          rows[n - 1].querySelector('.gut').appendChild(mark);
        }
      });
    }

    function fail(form, res) {
      var err = form.querySelector('.err');
      if (!err) {
        err = document.createElement('div');
        err.className = 'err';
        form.prepend(err);
      }
      res.text().then(function (t) {
        err.textContent = 'Not saved: ' + t;
      });
    }

    function url(path) {
      return '/c/' + encodeURIComponent(collection) + path;
    }

    document.addEventListener('submit', function (e) {
      var form = e.target;
      var text = form.elements.body ? form.elements.body.value : '';
      if (!form.classList.contains('newbox') && !form.classList.contains('reply') && !form.classList.contains('edit')) return;
      e.preventDefault();
      if (!text.trim()) return;
      var thread = form.closest('.thread');
      var lines = (thread || form).closest('.lines');
      var req;
      if (form.classList.contains('newbox')) {
        req = send('POST', url('/threads'), {
          version_id: Number(form.getAttribute('data-version')),
          first_line: Number(form.getAttribute('data-first')),
          last_line: Number(form.getAttribute('data-last')),
          body: text,
        });
      } else if (form.classList.contains('reply')) {
        req = send('POST', url('/threads/' + thread.getAttribute('data-thread') + '/comments'), { body: text });
      } else {
        var cid = form.closest('.cm').getAttribute('data-comment');
        req = send('PATCH', url('/comments/' + cid), { body: text });
      }
      req.then(function (res) {
        if (!res.ok) return fail(form, res);
        res.text().then(function (html) {
          if (form.classList.contains('newbox')) {
            box = null;
            drawMarks(lines, res);
            swap(form, html);
          } else {
            swap(thread, html);
          }
        });
      });
    });

    document.addEventListener('click', function (e) {
      var mark = e.target.closest('.mark');
      if (mark) {
        var t = document.getElementById('thread-' + mark.getAttribute('data-thread'));
        if (t) {
          t.hidden = !t.hidden;
          if (!t.hidden) t.scrollIntoView({ block: 'nearest' });
        }
        return;
      }
      var btn = e.target.closest('button[data-act]');
      if (!btn) return;
      var act = btn.getAttribute('data-act');
      if (act === 'cancel') {
        var edit = btn.closest('form.edit');
        if (edit) edit.replaceWith(edit._body);
        else closeBox();
      } else if (act === 'edit') {
        var cm = btn.closest('.cm');
        var bodyEl = cm.querySelector('.body');
        var form = document.createElement('form');
        form.className = 'edit';
        form._body = bodyEl;
        var area = document.createElement('textarea');
        area.name = 'body';
        area.rows = 3;
        area.value = bodyEl.textContent;
        var save = document.createElement('button');
        save.type = 'submit';
        save.textContent = 'Save';
        var cancel = document.createElement('button');
        cancel.type = 'button';
        cancel.setAttribute('data-act', 'cancel');
        cancel.textContent = 'Cancel';
        form.append(area, save, cancel);
        bodyEl.replaceWith(form);
        area.focus({ preventScroll: true });
      } else if (act === 'delete') {
        var thread = btn.closest('.thread');
        var lines = thread.closest('.lines');
        send('DELETE', url('/comments/' + btn.closest('.cm').getAttribute('data-comment'))).then(function (res) {
          if (!res.ok) return;
          if (res.status === 204) {
            drawMarks(lines, res);
            thread.remove();
          } else {
            res.text().then(function (html) {
              swap(thread, html);
            });
          }
        });
      }
    });
  }
})();

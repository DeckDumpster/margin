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

    function openBox(lines, first, last, cols) {
      closeBox();
      var form = document.createElement('form');
      form.className = 'newbox';
      form.setAttribute('data-first', first);
      form.setAttribute('data-last', last);
      form.setAttribute('data-version', lines.getAttribute('data-version'));
      if (cols) {
        form.setAttribute('data-start-col', cols[0]);
        form.setAttribute('data-end-col', cols[1]);
      }
      var who = document.createElement('div');
      who.className = 'who';
      who.textContent = cols
        ? (first === last ? 'Words on line ' + first : 'Words on lines ' + first + '\u2013' + last)
        : first === last ? 'Line ' + first : 'Lines ' + first + '\u2013' + last;
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
      var marks = JSON.parse(header);
      marks.forEach(function (m) {
        for (var n = m.first; n <= m.last; n++) {
          var mark = document.createElement('span');
          mark.className = 'mark';
          mark.setAttribute('data-thread', m.thread);
          mark.setAttribute('data-lane', m.lane);
          mark.style.setProperty('--lane', m.lane);
          rows[n - 1].querySelector('.gut').appendChild(mark);
        }
      });
      retint(lines, marks);
    }

    // ---- highlights of words -------------------------------------------------------------

    function scalars(str) {
      var n = 0;
      for (var i = 0; i < str.length; i++) {
        var c = str.charCodeAt(i);
        if (c >= 0xd800 && c < 0xdc00 && i + 1 < str.length) i++;
        n++;
      }
      return n;
    }

    // The UTF-16 index of the k-th scalar value of str.
    function unitsAt(str, k) {
      var i = 0;
      while (k > 0 && i < str.length) {
        var c = str.charCodeAt(i);
        i += c >= 0xd800 && c < 0xdc00 && i + 1 < str.length ? 2 : 1;
        k--;
      }
      return i;
    }

    // The runs of one line covered by some of spans [lane, start, end, thread]: the same cut as
    // margin-core's highlight::segments.
    function segments(len, spans) {
      var cuts = [];
      spans.forEach(function (s) {
        cuts.push(Math.min(s[1], len), Math.min(s[2], len));
      });
      cuts.sort(function (a, b) { return a - b; });
      var out = [];
      for (var i = 0; i + 1 < cuts.length; i++) {
        var a = cuts[i], b = cuts[i + 1];
        if (a === b) continue;
        var on = spans.filter(function (s) { return s[1] <= a && b <= s[2]; });
        if (!on.length) continue;
        var lanes = [], ids = [];
        on.sort(function (x, y) { return x[0] - y[0]; }).forEach(function (s) {
          if (lanes.indexOf(s[0]) < 0) {
            lanes.push(s[0]);
            ids.push(s[3]);
          }
        });
        var prev = out[out.length - 1];
        if (prev && prev.end === a && prev.lanes.join() === lanes.join()) prev.end = b;
        else out.push({ start: a, end: b, lanes: lanes, ids: ids });
      }
      return out;
    }

    // Wrap columns [from, to) of a code cell in a run, one wrapper per text node it crosses.
    function wrap(code, from, to, seg) {
      var walker = document.createTreeWalker(code, NodeFilter.SHOW_TEXT);
      var found = [];
      var col = 0;
      for (var node = walker.nextNode(); node; node = walker.nextNode()) {
        var len = scalars(node.data);
        if (col < to && col + len > from) found.push([node, Math.max(from - col, 0), Math.min(to - col, len)]);
        col += len;
      }
      found.forEach(function (f) {
        var node = f[0];
        var a = unitsAt(node.data, f[1]);
        var b = unitsAt(node.data, f[2]);
        var mid = a > 0 ? node.splitText(a) : node;
        if (b - a < mid.data.length) mid.splitText(b - a);
        var run = document.createElement('span');
        var lanes = [];
        seg.lanes.forEach(function (l) {
          var c = 'hl-l' + Math.min(l, 3);
          if (lanes.indexOf(c) < 0) lanes.push(c);
        });
        run.className = 'hl ' + lanes.join(' ');
        run.setAttribute('data-threads', seg.ids.join(' '));
        mid.replaceWith(run);
        run.appendChild(mid);
      });
    }

    // Redraw every tint of one document from its marks.
    function retint(lines, marks) {
      lines.querySelectorAll('.hl').forEach(function (h) {
        var parent = h.parentNode;
        while (h.firstChild) parent.insertBefore(h.firstChild, h);
        parent.removeChild(h);
        parent.normalize();
      });
      var rows = rowsOf(lines);
      var perLine = {};
      marks.forEach(function (m) {
        if (m.start_col === undefined || m.end_col === undefined) return;
        for (var n = m.first; n <= m.last; n++) {
          var len = scalars(rows[n - 1].querySelector('.code').textContent);
          var from = n === m.first ? m.start_col : 0;
          var to = n === m.last ? m.end_col : len;
          if (from < to) (perLine[n] = perLine[n] || []).push([m.lane, from, to, m.thread]);
        }
      });
      Object.keys(perLine).forEach(function (n) {
        var code = rows[n - 1].querySelector('.code');
        segments(scalars(code.textContent), perLine[n]).forEach(function (seg) {
          wrap(code, seg.start, seg.end, seg);
        });
      });
    }

    var cbtn = document.createElement('button');
    cbtn.type = 'button';
    cbtn.id = 'hl-comment';
    cbtn.textContent = 'Comment';
    cbtn.hidden = true;
    document.body.appendChild(cbtn);
    var pending = null; // {lines, first, last, cols} of the selection the button is offering

    function lineLen(row) {
      return scalars(row.querySelector('.code').textContent);
    }

    // Where a selection boundary falls, as {row, col}; null when it is not in this document's
    // lines. Boundaries in a line number, a thread or a form fall to the nearest code column.
    function locate(lines, node, offset, isEnd) {
      var top;
      var whole = node === lines;
      if (whole) top = lines.childNodes[isEnd ? offset - 1 : offset];
      else for (top = node; top && top.parentNode !== lines; ) top = top.parentNode;
      if (!top) return null;
      var isRow = top.nodeType === 1 && top.hasAttribute('data-line');
      if (isRow) {
        var code = top.querySelector('.code');
        if (!whole && code.contains(node)) {
          var r = document.createRange();
          r.setStart(code, 0);
          r.setEnd(node, offset);
          return { row: top, col: scalars(r.toString()) };
        }
        return { row: top, col: whole && isEnd ? lineLen(top) : 0 };
      }
      var near = isEnd ? 'previousElementSibling' : 'nextElementSibling';
      var row = top[near];
      while (row && !row.hasAttribute('data-line')) row = row[near];
      if (!row) return null;
      return { row: row, col: isEnd ? lineLen(row) : 0 };
    }

    // The selection as {lines, first, last, cols}, or null when it is not words of one document.
    function selectedWords() {
      var sel = window.getSelection();
      if (!sel || sel.rangeCount === 0 || sel.isCollapsed) return null;
      var range = sel.getRangeAt(0);
      var from = range.startContainer.nodeType === 1 ? range.startContainer : range.startContainer.parentNode;
      var to = range.endContainer.nodeType === 1 ? range.endContainer : range.endContainer.parentNode;
      var lines = from.closest('.lines');
      if (!lines || lines !== to.closest('.lines')) return null;
      var a = locate(lines, range.startContainer, range.startOffset, false);
      var b = locate(lines, range.endContainer, range.endOffset, true);
      if (!a || !b) return null;
      var n = function (p) { return Number(p.row.getAttribute('data-line')); };
      if (n(a) < n(b) && a.col >= lineLen(a.row)) a = { row: rowsOf(lines)[n(a)], col: 0 };
      if (n(b) > n(a) && b.col === 0) b = { row: rowsOf(lines)[n(b) - 2], col: lineLen(rowsOf(lines)[n(b) - 2]) };
      if (n(a) > n(b) || (n(a) === n(b) && a.col >= b.col)) return null;
      return { lines: lines, first: n(a), last: n(b), cols: [a.col, b.col], range: range };
    }

    function placeButton() {
      var w = pending && pending.range;
      var rects = w ? w.getClientRects() : [];
      var end = rects.length ? rects[rects.length - 1] : null;
      if (!end || end.bottom < 0 || end.top > window.innerHeight) {
        cbtn.hidden = true;
        return;
      }
      cbtn.hidden = false;
      var width = cbtn.offsetWidth;
      var height = cbtn.offsetHeight;
      cbtn.style.left = Math.max(8, Math.min(end.right, window.innerWidth - width - 8)) + 'px';
      cbtn.style.top = Math.max(8, Math.min(end.bottom + 8, window.innerHeight - height - 8)) + 'px';
    }

    document.addEventListener('selectionchange', function () {
      pending = selectedWords();
      if (!pending) cbtn.hidden = true;
      else placeButton();
    });
    window.addEventListener('scroll', function () {
      if (pending) placeButton();
    }, { passive: true });
    // Pressing the button must not collapse the selection it is about to read.
    cbtn.addEventListener('pointerdown', function (e) {
      e.preventDefault();
    });
    cbtn.addEventListener('mousedown', function (e) {
      e.preventDefault();
    });
    cbtn.addEventListener('click', function () {
      var p = pending || selectedWords();
      if (!p) return;
      window.getSelection().removeAllRanges();
      cbtn.hidden = true;
      pending = null;
      openBox(p.lines, p.first, p.last, p.cols);
    });

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
        var anchor = {
          version_id: Number(form.getAttribute('data-version')),
          first_line: Number(form.getAttribute('data-first')),
          last_line: Number(form.getAttribute('data-last')),
          body: text,
        };
        if (form.hasAttribute('data-start-col')) {
          anchor.start_col = Number(form.getAttribute('data-start-col'));
          anchor.end_col = Number(form.getAttribute('data-end-col'));
        }
        req = send('POST', url('/threads'), anchor);
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
      var hl = e.target.closest('.hl');
      if (hl && window.getSelection().isCollapsed) {
        var ids = hl.getAttribute('data-threads').split(' ').filter(Boolean);
        var all = ids
          .map(function (id) { return document.getElementById('thread-' + id); })
          .filter(Boolean);
        var show = all.some(function (t) { return t.hidden; });
        all.forEach(function (t) {
          t.hidden = !show;
        });
        if (show && all.length) all[0].scrollIntoView({ block: 'nearest' });
        return;
      }
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

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

    // The sticky bar: shown once the heading has scrolled away; its bottom is the visible top.
    var bar = document.querySelector('nav.bar');
    var barFile = document.getElementById('bar-file');
    var barPrev = document.getElementById('bar-prev');
    var barNext = document.getElementById('bar-next');

    function topEdge() {
      return bar && !bar.hidden ? bar.getBoundingClientRect().bottom : 0;
    }

    function docs() {
      return Array.prototype.slice.call(document.querySelectorAll('section.doc'));
    }

    // The last document whose top is at or above the bar's bottom (1px tolerance).
    function currentIndex(list) {
      var edge = topEdge() + 1;
      var at = -1;
      list.forEach(function (d, i) {
        if (d.getBoundingClientRect().top <= edge) at = i;
      });
      return at;
    }

    function pathOf(section) {
      var h = section.querySelector('.path');
      return h && h.firstChild ? h.firstChild.textContent.trim() : '';
    }

    function updateBar() {
      if (!bar) return;
      var header = document.querySelector('header.top');
      var show = !!header && header.getBoundingClientRect().bottom <= 0;
      bar.hidden = !show;
      var h1 = document.querySelector('h1.title');
      var title = bar.querySelector('.bar-title');
      if (h1 && title.textContent !== h1.textContent) title.textContent = h1.textContent;
      if (!show) return;
      var list = docs();
      var i = currentIndex(list);
      var full = i >= 0 ? pathOf(list[i]) : '';
      barFile.textContent = full.split('/').pop();
      if (full) barFile.title = full;
      else barFile.removeAttribute('title');
      barPrev.disabled = i < 1;
      barNext.disabled = i + 1 >= list.length;
    }

    if (bar) {
      window.addEventListener('scroll', updateBar, { passive: true });
      window.addEventListener('resize', updateBar);
      document.getElementById('bar-top').addEventListener('click', function () {
        window.scrollTo(0, 0);
      });
      var jump = function (by) {
        var list = docs();
        var target = list[currentIndex(list) + by];
        if (target) target.scrollIntoView();
      };
      barPrev.addEventListener('click', function () { jump(-1); });
      barNext.addEventListener('click', function () { jump(1); });
      updateBar();
    }

    function rowsOf(lines) {
      if (!lines._rows) lines._rows = Array.prototype.slice.call(lines.querySelectorAll('[data-line]'));
      return lines._rows;
    }

    function lineAt(lines, x, y) {
      var el = document.elementFromPoint(x, Math.max(y, topEdge() + 1));
      var row = el && el.closest('[data-line]');
      return row && lines.contains(row) ? Number(row.getAttribute('data-line')) : null;
    }

    function paint(lines, a, b, on) {
      var rows = rowsOf(lines);
      for (var n = Math.min(a, b); n <= Math.max(a, b); n++) rows[n - 1].classList.toggle('sel', on);
    }

    function whoText(first, last, words) {
      return words
        ? (first === last ? 'Words on line ' + first : 'Words on lines ' + first + '\u2013' + last)
        : first === last ? 'Line ' + first : 'Lines ' + first + '\u2013' + last;
    }

    function openBox(lines, first, last, cols) {
      var open = Array.prototype.slice.call(document.querySelectorAll('form.newbox'));
      var same = null;
      open.forEach(function (f) {
        if (!f.elements.body.value.trim()) f.remove();
        else if (
          lines.contains(f) &&
          f.getAttribute('data-first') === String(first) &&
          f.getAttribute('data-last') === String(last) &&
          f.getAttribute('data-start-col') === (cols ? String(cols[0]) : null) &&
          f.getAttribute('data-end-col') === (cols ? String(cols[1]) : null)
        ) same = f;
      });
      if (same) {
        same.elements.body.focus({ preventScroll: true });
        return;
      }
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
      who.textContent = whoText(first, last, !!cols);
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
      var top = topEdge();
      if (g.y < top + EDGE) dy = -MAX_SPEED * Math.min(1, (top + EDGE - g.y) / EDGE);
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
        t0: e.timeStamp,
        timer: setTimeout(beginRange, HOLD_MS),
      };
    });

    // Judged by event time, not by when the page got to the event: a move stamped before the hold
    // and past the slop is a swipe even if the hold timer has fired since.
    function movedEarly(e) {
      return (
        g.ranging &&
        e.timeStamp - g.t0 < HOLD_MS &&
        (Math.abs(e.clientX - g.x0) > SLOP || Math.abs(e.clientY - g.y0) > SLOP)
      );
    }

    document.addEventListener('pointermove', function (e) {
      if (!g || e.pointerId !== g.id) return;
      g.x = e.clientX;
      g.y = e.clientY;
      if (movedEarly(e)) {
        stopGesture(); // the hold timer fired before this move was handled, but the move came first
        return;
      }
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
      if (movedEarly(e)) {
        stopGesture();
        return;
      }
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

    // A tap on a number must not pull focus off the box the tap just focused.
    document.addEventListener('mousedown', function (e) {
      if (e.target.closest && e.target.closest('.no')) e.preventDefault();
    });

    document.addEventListener('contextmenu', function (e) {
      if (e.target.closest && e.target.closest('.no')) e.preventDefault();
    });

    function send(method, url, payload, signal) {
      return fetch(url, {
        method: method,
        headers: { 'content-type': 'application/json' },
        body: payload === undefined ? undefined : JSON.stringify(payload),
        signal: signal,
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
      if (header) drawMarkList(lines, JSON.parse(header));
    }

    function drawMarkList(lines, list) {
      lines.querySelectorAll('.mark').forEach(function (m) {
        m.remove();
      });
      var rows = rowsOf(lines);
      lines.querySelectorAll('.ln[style*="--lanes"]').forEach(function (r) {
        r.style.removeProperty('--lanes');
      });
      list.forEach(function (m) {
        for (var n = m.first; n <= m.last; n++) {
          if (m.lane + 1 > 4 && m.lane + 1 > Number(rows[n - 1].style.getPropertyValue('--lanes') || 0)) {
            rows[n - 1].style.setProperty('--lanes', m.lane + 1);
          }
          var mark = document.createElement('span');
          mark.className = 'mark';
          mark.setAttribute('data-thread', m.thread);
          mark.setAttribute('data-lane', m.lane);
          mark.style.setProperty('--lane', m.lane);
          rows[n - 1].querySelector('.gut').appendChild(mark);
        }
      });
      retint(lines, list);
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
      var top = topEdge();
      if (!end || end.bottom < top || end.top > window.innerHeight) {
        cbtn.hidden = true;
        return;
      }
      cbtn.hidden = false;
      var width = cbtn.offsetWidth;
      var height = cbtn.offsetHeight;
      cbtn.style.left = Math.max(8, Math.min(end.right, window.innerWidth - width - 8)) + 'px';
      cbtn.style.top = Math.max(top + 8, Math.min(end.bottom + 8, window.innerHeight - height - 8)) + 'px';
    }

    document.addEventListener('selectionchange', function () {
      pending = selectedWords();
      if (!pending) cbtn.hidden = true;
      else placeButton();
    });
    window.addEventListener('scroll', function () {
      if (pending) placeButton();
    }, { passive: true });
    // Pressing the button must not collapse the selection it is about to read. Only mousedown
    // is stopped: a preventDefault on pointerdown makes WebKit drop the click of a touch tap.
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

    function fail(form, message) {
      var err = form.querySelector('.err');
      if (!err) {
        err = document.createElement('div');
        err.className = 'err';
        form.prepend(err);
      }
      err.textContent = 'Not saved: ' + message;
    }

    // A form with a request in flight cannot send another; any failure puts it back.
    function setBusy(form, on) {
      var area = form.elements.body;
      var submit = form.querySelector('button[type=submit]');
      if (on) form.setAttribute('data-busy', '');
      else form.removeAttribute('data-busy');
      if (submit) submit.disabled = on;
      if (area) area.readOnly = on;
    }

    function url(path) {
      return '/c/' + encodeURIComponent(collection) + path;
    }

    document.addEventListener('submit', function (e) {
      var form = e.target;
      if (!form.classList.contains('newbox') && !form.classList.contains('reply') && !form.classList.contains('edit')) return;
      e.preventDefault();
      if (form.hasAttribute('data-busy')) return;
      var text = form.elements.body ? form.elements.body.value : '';
      if (!text.trim()) return;
      var thread = form.closest('.thread');
      var req;
      var signal = AbortSignal.timeout(5000);
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
        req = send('POST', url('/threads'), anchor, signal);
      } else if (form.classList.contains('reply')) {
        req = send('POST', url('/threads/' + thread.getAttribute('data-thread') + '/comments'), { body: text }, signal);
      } else {
        var cid = form.closest('.cm').getAttribute('data-comment');
        req = send('PATCH', url('/comments/' + cid), { body: text }, signal);
      }
      var stale = form.querySelector('.err');
      if (stale) stale.remove();
      setBusy(form, true);
      req
        .then(function (res) {
          return res.text().then(function (t) {
            return { res: res, text: t };
          });
        })
        .then(function (got) {
          var res = got.res;
          if (!document.contains(form)) {
            // Cancelled or replaced by the feed meanwhile: the feed brings the saved comment.
            return;
          }
          if (!res.ok) {
            setBusy(form, false);
            return fail(form, got.text);
          }
          if (form.classList.contains('newbox')) {
            drawMarks(form.closest('.lines'), res);
            swap(form, got.text);
          } else {
            // The feed may have replaced the thread while the request was out.
            swap(form.closest('.thread'), got.text);
          }
          recount();
        })
        .catch(function () {
          // No answer, or the answer could not be read or applied: the form is usable again.
          setBusy(form, false);
          if (document.contains(form)) fail(form, 'no answer from the server');
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
        else {
          var nb = btn.closest('form.newbox');
          if (nb) nb.remove();
        }
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
            recount();
          } else {
            res.text().then(function (html) {
              swap(thread, html);
              recount();
            });
          }
        });
      }
    });

    // Publish: the held comments appear through the feed below, not through this answer.
    var publish = document.getElementById('publish');
    if (publish) {
      publish.addEventListener('click', function () {
        publish.disabled = true;
        send('POST', url('/publish'))
          .then(function (res) {
            publish.textContent = res.ok ? 'Publish' : 'Not published: retry';
          })
          .catch(function () {
            publish.textContent = 'Not published: retry';
          })
          .then(function () {
            publish.disabled = false;
          });
      });
    }

    // Live changes: one long-poll that the server answers the moment something changes, then
    // asked again at once. A failed call is retried after a growing pause.
    var seq = Number(body.getAttribute('data-seq'));
    var failures = 0;

    // The reader's unposted text outlives a rewrite of the thread around it: its hidden state, its
    // reply text and an open edit move into `fresh`. `moved` collects old textarea -> new one, so
    // the caller can put the focus back.
    function transfer(old, fresh, moved) {
      fresh.hidden = old.hidden;
      // The reader's own reply form moves into the fresh thread, with its text, error, in-flight
      // state and response handler.
      var was = old.querySelector('form.reply');
      var now = fresh.querySelector('form.reply');
      if (was && now) now.replaceWith(was);
      var edit = old.querySelector('form.edit');
      var cm = edit && edit.closest('.cm');
      var body = cm && fresh.querySelector('.cm[data-comment="' + cm.getAttribute('data-comment') + '"] .body');
      if (body) {
        edit._body = body;
        body.replaceWith(edit);
      }
    }

    // The focused textarea and its selection, to be put back by restore() once its node has been
    // replaced or moved (which blurs it).
    function holdFocus() {
      var el = document.activeElement;
      var held = { moved: new Map(), restore: function () {} };
      if (!el || el.tagName !== 'TEXTAREA') return held;
      var sel = [el.selectionStart, el.selectionEnd, el.selectionDirection];
      held.restore = function () {
        var now = held.moved.get(el) || el;
        if (!now.isConnected) return;
        now.focus({ preventScroll: true });
        try {
          now.setSelectionRange(sel[0], sel[1], sel[2]);
        } catch (e) {}
      };
      return held;
    }

    function put(t) {
      var tpl = document.createElement('template');
      tpl.innerHTML = t.html.trim();
      var fresh = tpl.content.firstElementChild;
      var old = document.getElementById('thread-' + t.id);
      if (old) {
        var held = holdFocus();
        transfer(old, fresh, held.moved);
        old.replaceWith(fresh);
        held.restore();
        return;
      }
      var lines = document.querySelector('.lines[data-version="' + t.version_id + '"]');
      if (!lines) return;
      var at = rowsOf(lines)[t.last_line - 1];
      while (at.nextElementSibling && at.nextElementSibling.classList.contains('thread')) at = at.nextElementSibling;
      at.after(fresh);
    }

    // ---- a new version, or a change to the collection: the page refreshes in place ------------

    function carryOf(from, to, first, last, startCol, endCol) {
      var q = 'from=' + from + '&to=' + to + '&first_line=' + first + '&last_line=' + last;
      if (startCol !== null && startCol !== undefined) q += '&start_col=' + startCol + '&end_col=' + endCol;
      return fetch(url('/carry?' + q)).then(function (res) {
        if (res.status === 404 || res.status === 400) return null; // the version is gone: clamp
        if (!res.ok) throw new Error('carry answered ' + res.status);
        return res.json();
      });
    }

    function boxAnchor(form) {
      var has = form.hasAttribute('data-start-col');
      return [
        Number(form.getAttribute('data-version')),
        Number(form.getAttribute('data-first')),
        Number(form.getAttribute('data-last')),
        has ? Number(form.getAttribute('data-start-col')) : null,
        has ? Number(form.getAttribute('data-end-col')) : null,
      ];
    }

    // The first row whose bottom is below the top of the screen, if it is on screen at all.
    function topRow() {
      var rows = document.querySelectorAll('.ln');
      var top = topEdge();
      var lo = 0;
      var hi = rows.length;
      while (lo < hi) {
        var mid = (lo + hi) >> 1;
        if (rows[mid].getBoundingClientRect().bottom > top) hi = mid;
        else lo = mid + 1;
      }
      var row = rows[lo];
      return row && row.getBoundingClientRect().top < window.innerHeight ? row : null;
    }

    function onScreen(el) {
      var r = el.getBoundingClientRect();
      return r.bottom > topEdge() && r.top < window.innerHeight;
    }

    // What to keep still while the page is rewritten: the focused form if it is on screen, else the
    // topmost visible line. {el, find}: `find` returns the same thing afterwards.
    function placeToHold(replaced) {
      var focus = document.activeElement;
      var form = focus && focus.closest && focus.closest('form');
      if (form && onScreen(form)) {
        var thread = form.classList.contains('reply') && form.closest('.thread');
        if (thread) return { el: thread, find: function () { return document.getElementById(thread.id); } };
        return { el: form, find: function () { return form; } };
      }
      var row = topRow();
      if (!row) return null;
      var section = row.closest('section.doc');
      var entry = replaced.filter(function (e) { return e.old === section; })[0];
      if (!entry) return { el: row, find: function () { return row; } };
      return { el: row, entry: entry, line: Number(row.getAttribute('data-line')) };
    }

    // Everything the swap needs, fetched: the page, and where each box and the held line land.
    function prepare() {
      return fetch(url(''), { cache: 'no-store' })
        .then(function (res) {
          if (!res.ok) throw new Error('page answered ' + res.status);
          return res.text();
        })
        .then(function (html) {
          var page = new DOMParser().parseFromString(html, 'text/html');
          var entries = [];
          var fresh = Array.prototype.slice.call(page.querySelectorAll('section.doc'));
          fresh.forEach(function (fs) {
            var old = document.getElementById(fs.id);
            var was = old && old.querySelector('.lines').getAttribute('data-version');
            if (old && was === fs.querySelector('.lines').getAttribute('data-version')) {
              entries.push({ old: old, keep: true });
            } else {
              entries.push({ old: old, fresh: document.importNode(fs, true) });
            }
          });
          var replaced = entries.filter(function (e) { return e.old && !e.keep; });
          var plan = {
            page: page,
            entries: entries,
            gone: Array.prototype.filter.call(document.querySelectorAll('section.doc'), function (o) {
              return !page.getElementById(o.id);
            }),
            answers: new Map(),
            hold: placeToHold(replaced),
          };
          function missing() {
            var out = [];
            replaced.forEach(function (e) {
              e.old.querySelectorAll('form.newbox').forEach(function (f) {
                if (!plan.answers.has(f)) out.push([f, e]);
              });
            });
            var h = plan.hold;
            if (h && h.entry && !plan.answers.has(h)) out.push([h, h.entry]);
            return out;
          }
          // A box opened while these calls are in flight is asked about on the next round, before
          // anything is swapped.
          function settle() {
            var todo = missing();
            if (!todo.length) return plan;
            return Promise.all(
              todo.map(function (pair) {
                var key = pair[0];
                var from = pair[1].old.querySelector('.lines').getAttribute('data-version');
                var to = pair[1].fresh.querySelector('.lines').getAttribute('data-version');
                var a = key === plan.hold ? [from, plan.hold.line, plan.hold.line, null, null] : boxAnchor(key);
                return carryOf(from, to, a[1], a[2], a[3], a[4]).then(function (answer) {
                  plan.answers.set(key, answer);
                });
              })
            ).then(settle);
          }
          return settle();
        });
    }

    function clampTo(lines, n) {
      return Math.max(1, Math.min(n, rowsOf(lines).length));
    }

    // Move a box to where its anchor now sits in `lines`, the fresh document.
    function carryBox(form, lines, answer) {
      var first, last, cols, state;
      if (answer) {
        first = answer.first_line;
        last = answer.last_line;
        state = answer.state;
        cols = answer.start_col === undefined ? null : [answer.start_col, answer.end_col];
      } else {
        first = clampTo(lines, Number(form.getAttribute('data-first')));
        last = clampTo(lines, Number(form.getAttribute('data-last')));
        state = 'removed';
        cols = null;
      }
      form.setAttribute('data-version', lines.getAttribute('data-version'));
      form.setAttribute('data-first', first);
      form.setAttribute('data-last', last);
      if (cols) {
        form.setAttribute('data-start-col', cols[0]);
        form.setAttribute('data-end-col', cols[1]);
      } else {
        form.removeAttribute('data-start-col');
        form.removeAttribute('data-end-col');
      }
      var who = form.querySelector('.who');
      who.textContent = whoText(first, last, !!cols);
      var label = form.querySelector('.carry');
      var why = state === 'removed' ? 'on a line since removed' : state === 'text_changed' ? 'text since changed' : null;
      // A box already shown as removed or changed stays so, as a thread does across versions.
      if (!why && label && !answer) label.remove();
      if (!why && label && answer) why = label.textContent;
      if (why) {
        if (!label) {
          label = document.createElement('div');
          label.className = 'carry';
          who.before(label);
        }
        label.textContent = why;
      }
      rowsOf(lines)[last - 1].after(form);
    }

    // Swap the page for the fetched one, synchronously, keeping what the reader has written.
    function swapPage(plan, held) {
      var page = plan.page;
      document.title = page.title;
      var h1 = document.querySelector('h1.title');
      var fresh1 = page.querySelector('h1.title');
      if (h1 && fresh1) h1.textContent = fresh1.textContent;
      var meta = document.querySelector('.meta');
      var fmeta = page.querySelector('.meta');
      if (meta && fmeta) meta.textContent = fmeta.textContent;
      var files = document.querySelector('.files');
      var ffiles = page.querySelector('.files');
      if (files && ffiles) {
        files.replaceChildren.apply(files, Array.prototype.map.call(ffiles.children, function (a) {
          return document.importNode(a, true);
        }));
      }
      var cover = document.querySelector('.cover');
      var fcover = page.querySelector('.cover');
      if (fcover && cover) cover.replaceChildren.apply(cover, Array.prototype.map.call(fcover.childNodes, function (n) { return document.importNode(n, true); }));
      else if (fcover) (files || meta || document.querySelector('header.top')).after(document.importNode(fcover, true));
      else if (cover) cover.remove();
      if (publish) {
        var fpub = page.getElementById('publish');
        if (fpub) publish.hidden = fpub.hidden;
      }
      plan.gone.forEach(function (o) {
        o.remove();
      });
      var prev = null;
      plan.entries.forEach(function (e) {
        if (e.keep) {
          prev = e.old;
          return;
        }
        var lines = e.fresh.querySelector('.lines');
        if (e.old) {
          if (g && e.old.contains(g.lines)) stopGesture();
          if (pending && e.old.contains(pending.lines)) {
            window.getSelection().removeAllRanges();
            pending = null;
            cbtn.hidden = true;
          }
          e.fresh.querySelectorAll('.thread').forEach(function (ft) {
            var ot = document.getElementById(ft.id);
            if (ot) transfer(ot, ft, held.moved);
          });
          e.old.querySelectorAll('form.newbox').forEach(function (form) {
            carryBox(form, lines, plan.answers.get(form) || null);
          });
          e.old.replaceWith(e.fresh);
        } else if (prev) {
          prev.after(e.fresh);
        } else {
          (document.querySelector('.cover') || document.querySelector('.files') || document.querySelector('.meta')).after(e.fresh);
        }
        prev = e.fresh;
      });
    }

    function holdPlace(plan) {
      var h = plan.hold;
      if (!h) return null;
      var top = h.el.getBoundingClientRect().top;
      return function () {
        var el = h.find ? h.find() : null;
        if (!h.find) {
          var e = h.entry;
          var lines = e.fresh.querySelector('.lines');
          var a = plan.answers.get(h);
          el = rowsOf(lines)[a ? a.first_line - 1 : clampTo(lines, h.line) - 1];
        }
        if (!el || !el.isConnected) return;
        var shift = el.getBoundingClientRect().top - top;
        if (shift) window.scrollBy(0, shift);
      };
    }

    function applyChanges(d) {
      d.threads.forEach(put);
      d.gone.forEach(function (id) {
        var t = document.getElementById('thread-' + id);
        if (t) t.remove();
      });
      d.marks.forEach(function (v) {
        var lines = document.querySelector('.lines[data-version="' + v.version_id + '"]');
        if (lines) drawMarkList(lines, v.marks);
      });
      if (publish) publish.hidden = d.state === 'closed';
      recount();
    }

    // Each row of the files list says how many comments its section holds.
    function recount() {
      var rows = document.querySelectorAll('.files a');
      Array.prototype.forEach.call(rows, function (a) {
        var sec = document.getElementById((a.getAttribute('href') || '').slice(1));
        var sum = a.querySelector('.fsum');
        if (!sec || !sum) return;
        var n = sec.querySelectorAll('.cm').length;
        sum.textContent = sum.textContent.replace(/ · .*$/, ' · ' + (n === 0 ? 'no comments' : n === 1 ? '1 comment' : n + ' comments'));
      });
    }

    // Nothing on the page changes until everything it needs has arrived; a failure leaves the page
    // as it was and the poll asks again.
    function apply(d) {
      if (!d.reload) {
        applyChanges(d);
        return Promise.resolve();
      }
      return prepare().then(function (plan) {
        var held = holdFocus();
        var restorePlace = holdPlace(plan);
        swapPage(plan, held);
        applyChanges(d);
        held.restore();
        if (restorePlace) restorePlace();
        updateBar(); // after the held line is back, or the bar names the wrong file for a moment
      });
    }

    function poll() {
      fetch(url('/changes?after=' + seq + '&wait=25'))
        .then(function (res) {
          if (!res.ok) throw new Error('feed answered ' + res.status);
          return res.json();
        })
        .then(function (d) {
          return apply(d).then(function () {
            seq = d.last_seq;
            failures = 0;
            poll();
          });
        })
        .catch(function () {
          failures++;
          setTimeout(poll, Math.min(30000, 1000 * Math.pow(2, failures - 1)));
        });
    }
    poll();
  }
})();

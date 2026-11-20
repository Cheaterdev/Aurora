/* Aurora page script. No framework: plain DOM, fetch against the JSON API,
   re-fetched HTML fragments for the board and the activity list, and one
   EventSource for live updates. Everything is event-delegated on `document`,
   so swapping a fragment needs no re-wiring. */
(function () {
  'use strict';

  var body = document.body;
  var page = body.dataset.page;
  var project = body.dataset.project || '';

  function $(selector, root) { return (root || document).querySelector(selector); }
  function $$(selector, root) { return Array.prototype.slice.call((root || document).querySelectorAll(selector)); }

  // ---- small helpers ----

  function toast(message, kind) {
    var holder = $('#toasts');
    if (!holder) { return; }
    var item = document.createElement('div');
    item.className = 'toast' + (kind === 'error' ? ' error' : '');
    item.textContent = message;
    holder.appendChild(item);
    setTimeout(function () { item.remove(); }, kind === 'error' ? 8000 : 4000);
  }

  function goSignIn() {
    location.href = '/sign-in?next=' + encodeURIComponent(location.pathname + location.search);
  }

  function api(method, path, payload) {
    var options = { method: method, credentials: 'same-origin', headers: { 'X-Requested-With': 'aurora' } };
    if (payload !== undefined) {
      options.headers['Content-Type'] = 'application/json';
      options.body = JSON.stringify(payload);
    }
    return fetch('/api/v1' + path, options).then(function (res) {
      return res.text().then(function (text) {
        var data = null;
        if (text) { try { data = JSON.parse(text); } catch (err) { data = null; } }
        if (res.status === 401) { goSignIn(); throw new Error('You are signed out.'); }
        if (!res.ok) { throw new Error((data && data.error) || 'The request failed (' + res.status + ').'); }
        return data;
      });
    }, function () {
      throw new Error('Could not reach the server.');
    });
  }

  function message(element, text, kind) {
    if (!element) { return; }
    element.textContent = text || '';
    element.classList.toggle('error', kind === 'error');
    element.classList.toggle('saved', kind === 'saved');
  }

  function fieldMessage(element) {
    var field = element.closest('.field');
    return field ? $('.field-msg', field) : null;
  }

  function colorize(root) {
    $$('[data-color]', root).forEach(function (el) {
      if (/^#[0-9a-fA-F]{6}$/.test(el.dataset.color)) {
        el.style.setProperty('--label-color', el.dataset.color);
      }
    });
  }

  function localizeTimes(root) {
    $$('time[data-ts]', root).forEach(function (el) {
      var date = new Date(Number(el.dataset.ts));
      if (!isNaN(date.getTime())) { el.textContent = date.toLocaleString(); }
    });
  }

  function fetchFragment(path) {
    return fetch(path, { credentials: 'same-origin', headers: { 'X-Requested-With': 'aurora' } }).then(function (res) {
      if (res.status === 401) { goSignIn(); return null; }
      if (!res.ok) { return null; }
      return res.text();
    }, function () { return null; });
  }

  function parseFragment(html) {
    var template = document.createElement('template');
    template.innerHTML = html;
    return template.content.firstElementChild;
  }

  // ---- board: refresh ----

  var dragging = null;
  var refreshTimer = null;
  var refreshWanted = false;

  function typingInBoard() {
    var active = document.activeElement;
    var board = $('#board');
    return !!(board && active && board.contains(active) && /^(INPUT|SELECT|TEXTAREA)$/.test(active.tagName));
  }

  function refreshBoard(force) {
    var board = $('#board');
    if (!board) { return Promise.resolve(); }
    if (!force && (dragging || typingInBoard())) { refreshWanted = true; return Promise.resolve(); }
    refreshWanted = false;
    return fetchFragment('/p/' + encodeURIComponent(project) + '/board').then(function (html) {
      var current = $('#board');
      if (!html || !current) { return; }
      var fresh = parseFragment(html);
      if (!fresh) { return; }
      var left = current.scrollLeft;
      current.replaceWith(fresh);
      fresh.scrollLeft = left;
      colorize(fresh);
    });
  }

  function scheduleBoardRefresh() {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(function () { refreshBoard(false); }, 150);
  }

  document.addEventListener('focusout', function () {
    if (refreshWanted) { setTimeout(function () { if (refreshWanted) { refreshBoard(false); } }, 50); }
  });

  function updateCounts() {
    $$('#board .column').forEach(function (column) {
      var count = $('.count', column);
      if (count) { count.textContent = String($$('.card', column).length); }
    });
  }

  // ---- board: drag and drop ----

  function cardAfter(list, y) {
    var cards = $$('.card:not(.dragging)', list);
    for (var i = 0; i < cards.length; i += 1) {
      var box = cards[i].getBoundingClientRect();
      if (y < box.top + box.height / 2) { return cards[i]; }
    }
    return null;
  }

  function clearDropTargets() {
    $$('#board .column.drop-target').forEach(function (el) { el.classList.remove('drop-target'); });
  }

  function moveBody(card) {
    var column = card.closest('.column');
    var payload = { column_id: Number(column.dataset.columnId) };
    var next = card.nextElementSibling;
    if (next && next.classList.contains('card')) { payload.before = Number(next.dataset.number); }
    return payload;
  }

  function commitMove(card, origin) {
    var list = card.parentNode;
    if (list === origin.parent && card.nextElementSibling === origin.next) { return; }
    var payload = moveBody(card);
    card.dataset.columnId = String(payload.column_id);
    var select = $('.move-select', card);
    if (select) { select.value = String(payload.column_id); }
    updateCounts();
    api('POST', '/projects/' + encodeURIComponent(project) + '/issues/' + card.dataset.number + '/move', payload)
      .catch(function (err) {
        toast(err.message, 'error');
        return refreshBoard(true);
      });
  }

  document.addEventListener('dragstart', function (event) {
    var card = event.target.closest && event.target.closest('#board .card');
    if (!card) { return; }
    dragging = { card: card, parent: card.parentNode, next: card.nextElementSibling, dropped: false };
    card.classList.add('dragging');
    event.dataTransfer.effectAllowed = 'move';
    // Firefox starts no drag without data.
    event.dataTransfer.setData('text/plain', card.dataset.number);
  });

  document.addEventListener('dragover', function (event) {
    if (!dragging) { return; }
    var column = event.target.closest && event.target.closest('#board .column');
    if (!column) { return; }
    event.preventDefault();
    event.dataTransfer.dropEffect = 'move';
    clearDropTargets();
    column.classList.add('drop-target');
    var list = $('.cards', column);
    var after = cardAfter(list, event.clientY);
    if (dragging.card.nextElementSibling !== after || dragging.card.parentNode !== list) {
      list.insertBefore(dragging.card, after);
    }
  });

  document.addEventListener('drop', function (event) {
    if (!dragging) { return; }
    var column = event.target.closest && event.target.closest('#board .column');
    if (!column) { return; }
    event.preventDefault();
    dragging.dropped = true;
    commitMove(dragging.card, dragging);
  });

  document.addEventListener('dragend', function () {
    if (!dragging) { return; }
    var done = dragging;
    dragging = null;
    done.card.classList.remove('dragging');
    clearDropTargets();
    if (!done.dropped) {
      // Dropped outside any column: put the card back where it was.
      done.parent.insertBefore(done.card, done.next);
      updateCounts();
    }
    if (refreshWanted) { scheduleBoardRefresh(); }
  });

  // "Move to" select on every card: the way to move on a phone.
  document.addEventListener('change', function (event) {
    var select = event.target.closest && event.target.closest('.move-select');
    if (!select) { return; }
    var path = '/projects/' + encodeURIComponent(project) + '/issues/' + select.dataset.number + '/move';
    select.disabled = true;
    api('POST', path, { column_id: Number(select.value) })
      .catch(function (err) { toast(err.message, 'error'); })
      .then(function () { return refreshBoard(true); });
  });

  // ---- buttons and forms ----

  function showAddForm(button) {
    var form = $('.add-form', button.parentNode);
    if (!form) { return; }
    button.hidden = true;
    form.hidden = false;
    $('input[name="title"]', form).focus();
  }

  function hideAddForm(form) {
    form.hidden = true;
    var button = $('.add-btn', form.parentNode);
    if (button) { button.hidden = false; button.focus(); }
    message($('.field-msg', form), '');
  }

  document.addEventListener('click', function (event) {
    var button = event.target.closest && event.target.closest('[data-action]');
    if (!button) { return; }
    var action = button.dataset.action;
    if (action === 'show-add') {
      showAddForm(button);
    } else if (action === 'hide-add') {
      hideAddForm(button.closest('form'));
    } else if (action === 'sign-out') {
      api('POST', '/auth/sign-out').then(function () { location.href = '/sign-in'; },
        function (err) { toast(err.message, 'error'); });
    } else if (action === 'archive' || action === 'unarchive') {
      issueAction(button, action);
    } else if (action === 'save-description') {
      saveField($('#f-description'));
    }
  });

  document.addEventListener('keydown', function (event) {
    if (event.key !== 'Escape') { return; }
    var form = event.target.closest && event.target.closest('.add-form');
    if (form) { hideAddForm(form); }
  });

  function withBusy(form, work) {
    var buttons = $$('button', form);
    buttons.forEach(function (b) { b.disabled = true; });
    var out = $('.field-msg', form);
    message(out, '');
    return work().then(function () {
      buttons.forEach(function (b) { b.disabled = false; });
    }, function (err) {
      buttons.forEach(function (b) { b.disabled = false; });
      message(out, err.message, 'error');
    });
  }

  var formHandlers = {
    'add-issue': function (form) {
      var payload = { title: form.elements.title.value.trim(), column_id: Number(form.dataset.columnId) };
      if (form.elements.type_id) { payload.type_id = Number(form.elements.type_id.value); }
      return api('POST', '/projects/' + encodeURIComponent(project) + '/issues', payload).then(function () {
        form.reset();
        hideAddForm(form);
        return refreshBoard(true);
      });
    },
    'new-project': function (form) {
      var key = form.elements.key.value.trim();
      var name = form.elements.name.value.trim();
      return api('POST', '/projects', { key: key, name: name }).then(function () {
        // A new project has no issue types; start it with two so issues can be added.
        var base = '/projects/' + encodeURIComponent(key) + '/issue-types';
        return api('POST', base, { name: 'Task' }).then(function () {
          return api('POST', base, { name: 'Bug' });
        });
      }).then(function () { location.href = '/p/' + encodeURIComponent(key); });
    },
    'new-type': function (form) {
      return api('POST', '/projects/' + encodeURIComponent(project) + '/issue-types',
        { name: form.elements.name.value.trim() }).then(function () { location.reload(); });
    },
    'new-label': function (form) {
      return api('POST', '/projects/' + encodeURIComponent(project) + '/labels',
        { name: form.elements.name.value.trim(), color: form.elements.color.value }).then(function () { location.reload(); });
    }
  };

  document.addEventListener('submit', function (event) {
    var form = event.target.closest && event.target.closest('form[data-form]');
    if (!form || !formHandlers[form.dataset.form]) { return; }
    event.preventDefault();
    withBusy(form, function () { return formHandlers[form.dataset.form](form); });
  });

  document.addEventListener('input', function (event) {
    if (event.target.id === 'project-key') { event.target.value = event.target.value.toUpperCase(); }
  });

  // ---- issue page ----

  function issuePath() {
    return '/projects/' + encodeURIComponent(project) + '/issues/' + body.dataset.issueNumber;
  }

  function nullableNumber(value) { return value === '' ? null : Number(value); }

  function currentValue(holder) {
    if (holder.dataset.field === 'labels') {
      return JSON.stringify($$('input[type="checkbox"]:checked', holder).map(function (c) { return Number(c.value); }));
    }
    return holder.value;
  }

  function remember(holder) { holder.dataset.saved = currentValue(holder); }

  function revert(holder) {
    if (holder.dataset.field === 'labels') {
      var saved = JSON.parse(holder.dataset.saved || '[]');
      $$('input[type="checkbox"]', holder).forEach(function (c) { c.checked = saved.indexOf(Number(c.value)) !== -1; });
    } else {
      holder.value = holder.dataset.saved;
    }
  }

  function request(holder) {
    var field = holder.dataset.field;
    var value = holder.value;
    if (field === 'column') {
      return api('POST', issuePath() + '/move', { column_id: Number(value) });
    }
    var payload = {};
    if (field === 'title' || field === 'description') {
      payload[field] = value;
    } else if (field === 'type_id') {
      payload.type_id = Number(value);
    } else if (field === 'priority') {
      payload.priority = value === '' ? null : value;
    } else if (field === 'assignee_id' || field === 'parent') {
      payload[field] = nullableNumber(value);
    } else if (field === 'labels') {
      payload.labels = JSON.parse(currentValue(holder));
    }
    return api('PATCH', issuePath(), payload);
  }

  function saveField(holder) {
    if (!holder || currentValue(holder) === holder.dataset.saved) { return; }
    var out = fieldMessage(holder);
    message(out, 'Saving…');
    request(holder).then(function () {
      remember(holder);
      message(out, 'Saved', 'saved');
      setTimeout(function () { if (out && out.classList.contains('saved')) { message(out, ''); } }, 2500);
    }, function (err) {
      revert(holder);
      message(out, err.message, 'error');
    });
  }

  document.addEventListener('change', function (event) {
    if (page !== 'issue') { return; }
    var holder = event.target.closest && event.target.closest('[data-field]');
    if (holder) { saveField(holder); }
  });

  function issueAction(button, action) {
    button.disabled = true;
    api('POST', issuePath() + '/' + action).then(function () { location.reload(); }, function (err) {
      button.disabled = false;
      message($('#page-msg'), err.message, 'error');
    });
  }

  function refreshActivity() {
    return fetchFragment('/p/' + encodeURIComponent(project) + '/' + body.dataset.issueNumber + '/activity').then(function (html) {
      var current = $('#activity');
      if (!html || !current) { return; }
      var fresh = parseFragment(html);
      if (!fresh) { return; }
      current.replaceWith(fresh);
      localizeTimes(fresh);
    });
  }

  // ---- live updates ----

  var lastEvent = Number(body.dataset.lastEvent || 0);
  var source = null;

  function onEvent(event) {
    var data;
    try { data = JSON.parse(event.data); } catch (err) { return; }
    if (typeof data.id === 'number') { lastEvent = Math.max(lastEvent, data.id); }
    if (page === 'board') {
      scheduleBoardRefresh();
    } else if (page === 'issue' && data.entity_kind === 'issue' && String(data.entity_id) === body.dataset.issueId) {
      refreshActivity();
    }
  }

  function connect() {
    if (!window.EventSource || !project || (page !== 'board' && page !== 'issue')) { return; }
    var url = '/events/stream?after=' + lastEvent + '&project=' + encodeURIComponent(project);
    source = new EventSource(url);
    source.onmessage = onEvent;
    source.onerror = function () {
      // The browser retries on its own (resuming with Last-Event-ID) unless the
      // server answered with an error; then start over from the last id seen.
      if (source && source.readyState === EventSource.CLOSED) {
        source = null;
        setTimeout(connect, 3000);
      }
    };
  }

  window.addEventListener('pagehide', function () { if (source) { source.close(); source = null; } });

  // ---- start ----

  colorize(document);
  localizeTimes(document);
  $$('[data-field]').forEach(remember);
  connect();
}());

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
        if (!res.ok) {
          var failure = new Error((data && data.error) || 'The request failed (' + res.status + ').');
          failure.status = res.status;
          throw failure;
        }
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
    } else if (action === 'remove-link') {
      removeLink(button);
    } else if (action === 'show-reply' || action === 'show-edit') {
      showCommentForm(button, action === 'show-reply' ? '.reply-form' : '.comment-edit');
    } else if (action === 'hide-comment-form') {
      hideCommentForm(button.closest('form'));
    } else if (action === 'delete-comment') {
      deleteComment(button);
    } else if (action === 'delete-initiative') {
      deleteInitiative(button);
    } else if (action === 'copy-key') {
      copyKey(button);
    } else if (action === 'enable-agent' || action === 'disable-agent') {
      agentAction(button, function () {
        return api('POST', agentPath() + '/' + (action === 'enable-agent' ? 'enable' : 'disable'));
      }, function () { location.reload(); });
    } else if (action === 'delete-agent') {
      if (!window.confirm('Delete this agent? Its user and keys stay; the registry entry goes.')) { return; }
      agentAction(button, function () { return api('DELETE', agentPath()); },
        function () { location.href = '/agents'; });
    } else if (action === 'new-key') {
      agentAction(button, function () {
        return api('POST', '/keys', { name: 'agent key', user_id: Number(button.dataset.userId) });
      }, function (made) {
        button.disabled = false;
        showKey(made.key);
      });
    } else if (action === 'revoke-key') {
      agentAction(button, function () { return api('DELETE', '/keys/' + button.dataset.keyId); },
        function () { location.reload(); });
    } else if (action === 'edit-document') {
      editDocument(true);
    } else if (action === 'cancel-edit') {
      editDocument(false);
    } else if (action === 'preview-document') {
      previewDocument(button);
    } else if (action === 'archive-document' || action === 'unarchive-document') {
      archiveDocument(button, action === 'archive-document' ? 'archive' : 'unarchive');
    } else if (action === 'remove-doc-link') {
      removeDocumentLink(button);
    } else if (action === 'restore-revision') {
      restoreRevision(button);
    } else if (action === 'edit-template') {
      editTemplate(button);
    } else if (action === 'delete-template') {
      if (!window.confirm('Delete this override? The agent falls back to the next template.')) { return; }
      agentAction(button, function () {
        return api('DELETE', agentPath() + '/templates/' + button.dataset.templateId);
      }, function () { location.reload(); });
    } else if (action === 'clear-template-form') {
      clearTemplateForm(button.closest('form'));
    } else if (action === 'insert-variable') {
      insertVariable(button.dataset.text);
    } else if (action === 'preview-prompt') {
      previewPrompt(button);
    } else if (action === 'reload-page') {
      location.reload();
    } else if (action === 'preview-run') {
      previewRun(button);
    } else if (action === 'cancel-run') {
      cancelRun(button);
    } else if (action === 'more-output') {
      var panel = button.closest('.run-output');
      if (panel && panel.outputState) { button.hidden = true; outputFetch(panel.outputState); }
    }
  });

  document.addEventListener('keydown', function (event) {
    if (event.key !== 'Escape') { return; }
    var form = event.target.closest && event.target.closest('.add-form');
    if (form) { hideAddForm(form); return; }
    var commentForm = event.target.closest && event.target.closest('.reply-form, .comment-edit');
    if (commentForm) { hideCommentForm(commentForm); }
  });

  // `/` jumps to the search box unless the person is typing somewhere.
  document.addEventListener('keydown', function (event) {
    if (event.key !== '/' || event.ctrlKey || event.metaKey || event.altKey) { return; }
    var target = event.target;
    if (target && (/^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName) || target.isContentEditable)) { return; }
    var box = document.getElementById('search-q');
    if (!box) { return; }
    event.preventDefault();
    box.focus();
    box.select();
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

  // ---- agents ----

  function agentPayload(form, creating) {
    var e = form.elements;
    var payload = {
      description: e.description.value.trim(),
      command: e.command.value.trim(),
      args: e.args.value.split(/\r?\n/).filter(function (line) { return line !== ''; }),
      instructions: e.instructions.value,
      max_concurrent: Number(e.max_concurrent.value),
      timeout_seconds: Number(e.timeout_seconds.value),
      on_assign: e.on_assign.checked,
      on_mention: e.on_mention.checked,
      projects: $$('input[name="project"]:checked', form).map(function (box) { return box.value; })
    };
    ['model', 'work_dir'].forEach(function (name) {
      var text = e[name].value.trim();
      if (text !== '') { payload[name] = text; } else if (!creating) { payload[name] = null; }
    });
    return payload;
  }

  // The key goes into the page only here, as text, and is never put in a URL.
  function showKey(secret) {
    var box = $('#key-box');
    if (!box) { return; }
    $('#key-value', box).textContent = secret;
    $('#key-command', box).textContent = 'claude mcp add --transport http aurora ' + location.origin +
      '/mcp --header "Authorization: Bearer ' + secret + '"';
    box.hidden = false;
    box.scrollIntoView({ block: 'center' });
  }

  function copyKey(button) {
    var secret = $('#key-value').textContent;
    if (!navigator.clipboard) { toast('Select the key and copy it by hand.', 'error'); return; }
    navigator.clipboard.writeText(secret).then(function () { button.textContent = 'Copied'; },
      function () { toast('Select the key and copy it by hand.', 'error'); });
  }

  function agentPath() { return '/agents/' + $('[data-agent-id]').dataset.agentId; }

  function agentAction(button, run, done) {
    button.disabled = true;
    run().then(done, function (err) {
      button.disabled = false;
      message($('#page-msg'), err.message, 'error');
    });
  }

  // ---- prompt section of the agent page ----

  var lastPromptField = null;

  document.addEventListener('focusin', function (event) {
    if (event.target.matches && event.target.matches('[data-prompt-field]')) { lastPromptField = event.target; }
  });

  // Only the issue types of the chosen project can be picked.
  function filterTypes() {
    var chosen = $('#t-project');
    var types = $('#t-type');
    if (!chosen || !types) { return; }
    $$('option[data-project]', types).forEach(function (option) {
      var match = option.dataset.project === chosen.value;
      option.hidden = !match;
      option.disabled = !match;
    });
    var selected = types.options[types.selectedIndex];
    if (selected && selected.disabled) { types.value = ''; }
  }

  document.addEventListener('change', function (event) {
    if (event.target.id === 't-project') { filterTypes(); }
  });

  function editTemplate(button) {
    var form = $('form[data-form="save-template"]');
    if (!form) { return; }
    form.elements.project.value = button.dataset.project;
    filterTypes();
    form.elements.issue_type_id.value = button.dataset.typeId || '';
    form.elements.body.value = button.dataset.body;
    message($('.field-msg', form), '');
    form.elements.body.focus();
    lastPromptField = form.elements.body;
  }

  function clearTemplateForm(form) {
    form.elements.body.value = '';
    form.elements.issue_type_id.value = '';
    message($('.field-msg', form), '');
  }

  function insertVariable(text) {
    var field = lastPromptField && document.contains(lastPromptField) ? lastPromptField : $('#t-body');
    if (!field || !text) { return; }
    var start = field.selectionStart === undefined ? field.value.length : field.selectionStart;
    var end = field.selectionEnd === undefined ? start : field.selectionEnd;
    field.value = field.value.slice(0, start) + text + field.value.slice(end);
    var caret = start + text.length;
    field.focus();
    field.setSelectionRange(caret, caret);
    lastPromptField = field;
  }

  function previewPrompt(button) {
    var out = $('#pv-msg');
    var meta = $('#pv-meta');
    var shown = $('#pv-text');
    var payload = {
      project: $('#pv-project').value,
      number: Number($('#pv-number').value),
      trigger: $('#pv-trigger').value
    };
    var source = button.dataset.source;
    if (!payload.project || !payload.number) {
      message(out, 'Choose a project and an issue number.', 'error');
      return;
    }
    if (source !== 'stored') {
      payload.template = $(source).value;
      if (payload.template.trim() === '') {
        message(out, 'That text is empty. "Preview as saved" shows what applies now.', 'error');
        return;
      }
    }
    button.disabled = true;
    message(out, '');
    api('POST', agentPath() + '/prompt-preview', payload).then(function (made) {
      // textContent: the message holds issue and comment text and is never markup.
      shown.textContent = made.text;
      shown.hidden = false;
      meta.textContent = made.chars + ' characters, written from ' + made.template_source.replace('_', ' ') +
        (made.truncated ? '. Parts were shortened to fit the limits.' : '.');
      button.disabled = false;
    }, function (err) {
      shown.hidden = true;
      meta.textContent = '';
      message(out, err.message, 'error');
      button.disabled = false;
    });
  }

  // ---- runs ----

  function previewRun(button) {
    var form = button.closest('form');
    var area = form.elements.message;
    var out = $('.field-msg', form);
    var payload = { project: form.elements.project.value, number: Number(form.elements.number.value) };
    if (!payload.project || !payload.number) {
      message(out, 'Choose a project and an issue number.', 'error');
      return;
    }
    button.disabled = true;
    message(out, '');
    api('POST', agentPath() + '/prompt-preview', payload).then(function (made) {
      // The text is put in a textarea value, never parsed as markup.
      area.value = made.text;
      area.dataset.rendered = made.text;
      message(out, made.chars + ' characters, written from ' + made.template_source.replace('_', ' ') + '.', 'saved');
      button.disabled = false;
    }, function (err) {
      message(out, err.message, 'error');
      button.disabled = false;
    });
  }

  function refreshRun() {
    return fetchFragment('/p/' + encodeURIComponent(project) + '/runs/' + body.dataset.runId + '/live').then(function (html) {
      var current = $('#run-live');
      if (!html || !current) { return; }
      var fresh = parseFragment(html);
      if (!fresh) { return; }
      current.replaceWith(fresh);
      localizeTimes(fresh);
    });
  }

  function refreshRuns() {
    return fetchFragment('/p/' + encodeURIComponent(project) + '/' + body.dataset.issueNumber + '/runs').then(function (html) {
      var current = $('#runs');
      if (!html || !current) { return; }
      var fresh = parseFragment(html);
      if (!fresh) { return; }
      current.replaceWith(fresh);
      localizeTimes(fresh);
      syncIssueOutput();
    });
  }

  // No confirmation: the button is disabled at once and says what is going on.
  // The answer comes when the process tree is gone (at most a few seconds).
  function cancelRun(button) {
    if (button.disabled) { return; }
    var label = button.textContent;
    button.disabled = true;
    button.textContent = 'Canceling…';
    api('POST', '/runs/' + button.dataset.runId + '/cancel').then(function () {
      if (page === 'run') { refreshRun(); } else if (page === 'issue') { refreshRuns(); }
    }, function (err) {
      button.disabled = false;
      button.textContent = label;
      // The run may have ended on its own meanwhile: show what is true now.
      if (err.status === 409) {
        if (page === 'run') { refreshRun(); } else if (page === 'issue') { refreshRuns(); }
      }
      toast(err.message, 'error');
    });
  }

  // ---- output: the transcript of a run ----
  // Polling, no sockets: GET /runs/{id}/transcript?after=<next> every second
  // while the run is active (every 3 s once nothing new has come for a while),
  // until the answer says `done`. Every piece of text goes in with
  // textContent, never as markup. The server-rendered tail inside the panel is
  // what shows without script; it is replaced by the stored transcript here.

  var PAGE_BYTES = 262144;
  var MAX_RENDERED_CHARS = 1500000;

  function outputRole(panel, name) { return $('[data-role="' + name + '"]', panel); }

  function atBottom(view) { return view.scrollTop + view.clientHeight >= view.scrollHeight - 8; }

  function outputState(st, text) {
    var el = outputRole(st.panel, 'state');
    if (el) { el.textContent = text; }
  }

  function outputAppend(st, entries) {
    var view = outputRole(st.panel, 'view');
    var follow = outputRole(st.panel, 'follow');
    var stick = !!(follow && follow.checked);
    var fragment = document.createDocumentFragment();
    entries.forEach(function (entry) {
      var text = String(entry.d);
      var span = document.createElement('span');
      if (entry.s === 'e') {
        span.className = 't-err';
      } else if (entry.s === 'x') {
        span.className = 't-mark';
        text = (st.lastNewline ? '' : '\n') + '— ' + text + ' —\n';
      }
      span.textContent = text;
      st.lastNewline = text.charAt(text.length - 1) === '\n';
      st.chars += text.length;
      fragment.appendChild(span);
    });
    view.appendChild(fragment);
    while (st.chars > MAX_RENDERED_CHARS && view.firstChild && view.firstChild !== view.lastChild) {
      st.chars -= view.firstChild.textContent.length;
      view.removeChild(view.firstChild);
      st.trimmed = true;
    }
    if (stick) { view.scrollTop = view.scrollHeight; }
  }

  function outputShow(st, hasText) {
    var view = outputRole(st.panel, 'view');
    var empty = outputRole(st.panel, 'empty');
    if (view) { view.hidden = !hasText; }
    if (empty) {
      empty.hidden = hasText;
      empty.textContent = st.active ? 'Nothing yet.' : 'The process printed nothing.';
    }
  }

  function outputStop(st) {
    st.stopped = true;
    clearTimeout(st.timer);
  }

  function outputFetch(st) {
    if (st.stopped || st.loading) { return; }
    st.loading = true;
    api('GET', '/runs/' + st.runId + '/transcript?after=' + st.next + '&limit=' + PAGE_BYTES).then(function (data) {
      st.loading = false;
      if (st.stopped) { return; }
      if (!st.started) {
        st.started = true;
        // The stored transcript replaces the server-rendered tail.
        outputRole(st.panel, 'view').textContent = '';
        st.chars = 0;
      }
      if (data.entries.length > 0) { outputAppend(st, data.entries); st.idle = 0; } else { st.idle += 1; }
      st.next = data.next;
      st.done = data.done === true;
      outputShow(st, st.chars > 0);
      var note = [];
      if (data.truncated) { note.push('Output was cut at the size limit.'); }
      if (st.trimmed) { note.push('Showing the latest part; download for everything.'); }
      var more = outputRole(st.panel, 'more');
      if (st.done) {
        outputState(st, ['Finished.'].concat(note).join(' '));
        if (more) { more.hidden = true; }
        outputStop(st);
        return;
      }
      if (st.active) {
        outputState(st, ['Live.'].concat(note).join(' '));
        st.timer = setTimeout(function () { outputFetch(st); }, st.idle >= 3 ? 3000 : 1000);
      } else {
        // A finished run: pages are fetched on request.
        var hasMore = data.entries.length > 0 && !data.done;
        if (more) { more.hidden = !hasMore; }
        outputState(st, note.join(' '));
        if (!hasMore) { outputStop(st); }
      }
    }, function (err) {
      st.loading = false;
      if (st.stopped) { return; }
      if (err.status === 404) {
        st.started = true;
        outputRole(st.panel, 'view').textContent = '';
        st.chars = 0;
        outputShow(st, false);
        outputRole(st.panel, 'empty').textContent = 'No output was recorded for this run.';
        outputState(st, '');
        outputStop(st);
        return;
      }
      outputState(st, 'Could not load the output (' + err.message + ') — retrying.');
      if (st.active) { st.timer = setTimeout(function () { outputFetch(st); }, 3000); }
    });
  }

  function startOutput(panel, runId, active) {
    if (panel.outputState) { outputStop(panel.outputState); }
    var view = outputRole(panel, 'view');
    var st = {
      panel: panel, runId: runId, active: active, next: 0, chars: 0, idle: 0, started: false,
      loading: false, stopped: false, done: false, trimmed: false, lastNewline: true, timer: null
    };
    panel.outputState = st;
    panel.dataset.runId = String(runId);
    panel.dataset.active = active ? 'true' : 'false';
    var link = outputRole(panel, 'download');
    if (link) { link.href = '/api/v1/runs/' + runId + '/transcript.txt?download=1'; }
    panel.hidden = false;
    var label = outputRole(panel, 'follow-label');
    var follow = outputRole(panel, 'follow');
    if (label) { label.hidden = !active; }
    if (follow) { follow.checked = true; }
    if (view) {
      view.onscroll = function () { if (follow) { follow.checked = atBottom(view); } };
    }
    if (follow) {
      follow.onchange = function () { if (follow.checked && view) { view.scrollTop = view.scrollHeight; } };
    }
    outputState(st, 'Loading…');
    outputFetch(st);
  }

  function initOutput() {
    var panel = $('#run-output');
    if (!panel || !panel.dataset.runId) { return; }
    startOutput(panel, Number(panel.dataset.runId), panel.dataset.active === 'true');
  }

  // The issue page follows the newest active run of the issue.
  function syncIssueOutput() {
    var panel = $('#issue-output');
    var runs = $('#runs');
    if (!panel || !runs || !runs.dataset.liveRun) { return; }
    var wanted = Number(runs.dataset.liveRun);
    if (panel.outputState && panel.outputState.runId === wanted) { return; }
    startOutput(panel, wanted, true);
  }

  var formHandlers = {
    'start-run': function (form) {
      var area = form.elements.message;
      var payload = { issue: form.elements.project.value + '-' + Number(form.elements.number.value) };
      if (area.value.trim() !== '' && area.value !== area.dataset.rendered) { payload.message = area.value; }
      return api('POST', agentPath() + '/runs', payload).then(function (made) {
        location.href = '/p/' + encodeURIComponent(made.project) + '/runs/' + made.id;
      });
    },
    'save-template': function (form) {
      var payload = { project: form.elements.project.value, body: form.elements.body.value };
      if (form.elements.issue_type_id.value) { payload.issue_type_id = Number(form.elements.issue_type_id.value); }
      return api('PUT', agentPath() + '/templates', payload).then(function () { location.reload(); });
    },
    'new-agent': function (form) {
      var payload = agentPayload(form, true);
      payload.name = form.elements.name.value.trim();
      payload.create_key = form.elements.create_key.checked;
      return api('POST', '/agents', payload).then(function (made) {
        if (!made.key) { location.href = '/agents/' + made.id; return; }
        form.reset();
        showKey(made.key);
        $('.field-msg', form).textContent = 'Created. The agent is ';
        var link = document.createElement('a');
        link.href = '/agents/' + made.agent.id;
        link.textContent = made.agent.name;
        $('.field-msg', form).appendChild(link);
      });
    },
    'edit-agent': function (form) {
      return api('PATCH', agentPath(), agentPayload(form, false)).then(function () {
        message($('.field-msg', form), 'Saved', 'saved');
      });
    },
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
    'add-link': function (form) {
      var choice = form.elements.kind.value;
      var payload = {
        kind: choice === 'relates' ? 'relates' : 'blocks',
        to: Number(form.elements.to.value)
      };
      if (choice === 'blocked_by') { payload.direction = 'blocked_by'; }
      return api('POST', issuePath() + '/links', payload).then(function () { location.reload(); });
    },
    'add-comment': function (form) {
      return postComment(form, {});
    },
    'add-reply': function (form) {
      return postComment(form, { parent: Number(form.dataset.parentId) });
    },
    'edit-comment': function (form) {
      return api('PATCH', '/projects/' + encodeURIComponent(project) + '/comments/' + form.dataset.commentId,
        { body: form.elements.body.value }).then(function () { return refreshComments(true); });
    },
    'new-initiative': function (form) {
      var payload = { name: form.elements.name.value.trim() };
      if (form.elements.target_date.value) { payload.target_date = form.elements.target_date.value; }
      return api('POST', '/projects/' + encodeURIComponent(project) + '/initiatives', payload).then(function (made) {
        location.href = '/p/' + encodeURIComponent(project) + '/initiatives/' + made.id;
      });
    },
    'new-document': function (form) {
      var payload = { title: form.elements.title.value.trim() };
      if (form.elements.parent.value) { payload.parent = Number(form.elements.parent.value); }
      return api('POST', '/projects/' + encodeURIComponent(project) + '/documents', payload).then(function (made) {
        location.href = documentsPath() + '/' + made.id;
      });
    },
    'save-document': function (form) {
      var doc = $('#doc');
      var payload = {
        title: form.elements.title.value.trim(),
        body: form.elements.body.value,
        base_revision: Number(doc.dataset.revision)
      };
      return api('PATCH', documentApi(body.dataset.documentId), payload).then(function () {
        location.reload();
      }, function (err) {
        if (err.status === 409) {
          var reload = $('#doc-reload');
          if (reload) { reload.hidden = false; }
          throw new Error('Someone else changed this document since you opened it. Your text is still here; copy it, then reload to see the latest version.');
        }
        throw err;
      });
    },
    'add-doc-link': function (form) {
      var payload = {};
      payload[form.elements.kind.value] = Number(form.elements.target.value);
      return api('POST', documentApi(body.dataset.documentId) + '/links', payload).then(function () {
        location.reload();
      });
    },
    'new-label': function (form) {
      return api('POST', '/projects/' + encodeURIComponent(project) + '/labels',
        { name: form.elements.name.value.trim(), color: form.elements.color.value }).then(function () { location.reload(); });
    },
    'repo-settings': function (form) {
      var path = form.elements.repo_path.value.trim();
      var branch = form.elements.base_branch.value.trim();
      return api('PATCH', '/projects/' + encodeURIComponent(project), {
        repo_path: path === '' ? null : path,
        base_branch: branch === '' ? null : branch
      }).then(function (saved) {
        // The server keeps the repository's top-level folder.
        form.elements.repo_path.value = saved.repo_path || '';
        form.elements.base_branch.value = saved.base_branch || '';
        message($('.field-msg', form), 'Saved', 'saved');
      });
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

  function initiativePath() {
    return '/projects/' + encodeURIComponent(project) + '/initiatives/' + body.dataset.initiativeId;
  }

  function request(holder) {
    var field = holder.dataset.field;
    var value = holder.value;
    if (page === 'initiative') {
      var patch = {};
      patch[field] = field === 'target_date' && value === '' ? null : value;
      return api('PATCH', initiativePath(), patch);
    }
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
    } else if (field === 'assignee_id' || field === 'parent' || field === 'initiative') {
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
      var title = $('#initiative-title');
      if (page === 'initiative' && holder.dataset.field === 'name' && title) { title.textContent = holder.value.trim(); }
      message(out, 'Saved', 'saved');
      setTimeout(function () { if (out && out.classList.contains('saved')) { message(out, ''); } }, 2500);
    }, function (err) {
      revert(holder);
      message(out, err.message, 'error');
    });
  }

  document.addEventListener('change', function (event) {
    if (page !== 'issue' && page !== 'initiative') { return; }
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

  function removeLink(button) {
    button.disabled = true;
    message($('#links-msg'), '');
    api('DELETE', '/projects/' + encodeURIComponent(project) + '/links/' + button.dataset.linkId)
      .then(function () { location.reload(); }, function (err) {
        button.disabled = false;
        message($('#links-msg'), err.message, 'error');
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

  // ---- comments ----

  var commentsTimer = null;
  var commentsWanted = false;

  // A refresh would throw away what is being typed: wait until the writing is done.
  function composing() {
    return $$('#comments textarea').some(function (area) {
      var form = area.closest('form');
      return form && !form.hidden && (area === document.activeElement || area.value !== area.defaultValue);
    });
  }

  function refreshComments(force) {
    if (!$('#comments')) { return Promise.resolve(); }
    if (!force && composing()) { commentsWanted = true; return Promise.resolve(); }
    commentsWanted = false;
    return fetchFragment('/p/' + encodeURIComponent(project) + '/' + body.dataset.issueNumber + '/comments').then(function (html) {
      var current = $('#comments');
      if (!html || !current) { return; }
      var fresh = parseFragment(html);
      if (!fresh) { return; }
      current.replaceWith(fresh);
      localizeTimes(fresh);
    });
  }

  function scheduleCommentsRefresh() {
    clearTimeout(commentsTimer);
    commentsTimer = setTimeout(function () { refreshComments(false); }, 150);
  }

  document.addEventListener('focusout', function () {
    if (commentsWanted) { setTimeout(function () { if (commentsWanted) { refreshComments(false); } }, 50); }
  });

  function postComment(form, extra) {
    var payload = { body: form.elements.body.value };
    if (extra.parent) { payload.parent = extra.parent; }
    return api('POST', issuePath() + '/comments', payload).then(function () {
      form.reset();
      return refreshComments(true);
    });
  }

  function showCommentForm(button, selector) {
    var item = button.closest('.comment');
    var form = item && $(selector, item);
    if (!form) { return; }
    form.hidden = false;
    var area = $('textarea', form);
    if (area) { area.focus(); }
  }

  function hideCommentForm(form) {
    if (!form) { return; }
    var area = $('textarea', form);
    if (area) { area.value = area.defaultValue; }
    form.hidden = true;
    message($('.field-msg', form), '');
    if (commentsWanted) { refreshComments(false); }
  }

  function deleteComment(button) {
    if (!window.confirm('Delete this comment?')) { return; }
    var item = button.closest('.comment');
    button.disabled = true;
    api('DELETE', '/projects/' + encodeURIComponent(project) + '/comments/' + item.dataset.commentId)
      .then(function () { return refreshComments(true); }, function (err) {
        button.disabled = false;
        message($('#page-msg'), err.message, 'error');
      });
  }

  // ---- initiatives ----

  var initiativeTimer = null;

  function initiativesPath() { return '/p/' + encodeURIComponent(project) + '/initiatives'; }

  function deleteInitiative(button) {
    if (!window.confirm('Delete this initiative? Its issues stay, but leave the initiative.')) { return; }
    button.disabled = true;
    api('DELETE', initiativePath()).then(function () { location.href = initiativesPath(); }, function (err) {
      button.disabled = false;
      message($('#page-msg'), err.message, 'error');
    });
  }

  // The list or the progress section is swapped; the fields being edited are left alone.
  function refreshInitiative() {
    var onList = page === 'initiatives';
    var url = onList ? initiativesPath() + '/list' : initiativesPath() + '/' + body.dataset.initiativeId + '/progress';
    return fetchFragment(url).then(function (html) {
      var current = $(onList ? '#initiative-list' : '#initiative-progress');
      if (!html || !current) { return; }
      var fresh = parseFragment(html);
      if (fresh) { current.replaceWith(fresh); }
    });
  }

  function scheduleInitiativeRefresh() {
    clearTimeout(initiativeTimer);
    initiativeTimer = setTimeout(refreshInitiative, 150);
  }

  // ---- documents ----

  var documentTimer = null;

  function documentsPath() { return '/p/' + encodeURIComponent(project) + '/documents'; }

  function documentApi(id) { return '/projects/' + encodeURIComponent(project) + '/documents/' + id; }

  function editing() {
    var editor = $('#doc-editor');
    return !!(editor && !editor.hidden);
  }

  function editDocument(on) {
    var editor = $('#doc-editor');
    var view = $('#doc-body');
    if (!editor || !view) { return; }
    editor.hidden = !on;
    view.hidden = on;
    var preview = $('#doc-preview');
    if (preview) { preview.hidden = true; preview.textContent = ''; }
    if (on) {
      $('#doc-edit-body').focus();
    } else {
      editor.reset();
      message($('.field-msg', editor), '');
      if (documentWanted) { refreshDocument(); }
    }
  }

  function previewDocument(button) {
    var preview = $('#doc-preview');
    var area = $('#doc-edit-body');
    var out = $('.field-msg', button.closest('form'));
    if (!preview || !area) { return; }
    button.disabled = true;
    message(out, '');
    api('POST', '/projects/' + encodeURIComponent(project) + '/documents/preview', { body: area.value })
      .then(function (made) {
        // The server's renderer is the only producer of this markup; see markdown.rs.
        preview.innerHTML = made.html;
        preview.hidden = false;
        button.disabled = false;
      }, function (err) {
        button.disabled = false;
        message(out, err.message, 'error');
      });
  }

  function archiveDocument(button, action) {
    button.disabled = true;
    api('POST', documentApi(body.dataset.documentId) + '/' + action).then(function () { location.reload(); },
      function (err) {
        button.disabled = false;
        message($('#page-msg'), err.message, 'error');
      });
  }

  function removeDocumentLink(button) {
    var id = button.dataset.documentId || body.dataset.documentId;
    button.disabled = true;
    api('DELETE', documentApi(id) + '/links/' + button.dataset.linkId).then(function () { location.reload(); },
      function (err) {
        button.disabled = false;
        message($('#page-msg') || $('#links-msg'), err.message, 'error');
      });
  }

  function restoreRevision(button) {
    if (!window.confirm('Restore revision ' + button.dataset.revision + '? It becomes a new revision; nothing is lost.')) { return; }
    button.disabled = true;
    api('POST', documentApi(body.dataset.documentId) + '/restore',
      { revision: Number(button.dataset.revision), base_revision: Number(button.dataset.baseRevision) })
      .then(function () { location.href = documentsPath() + '/' + body.dataset.documentId; }, function (err) {
        button.disabled = false;
        message($('#page-msg'), err.status === 409
          ? 'The document changed since this page was opened. Reload and try again.' : err.message, 'error');
      });
  }

  var documentWanted = false;

  // The tree or the document is swapped; a document being edited is left alone.
  function refreshDocument() {
    var onList = page === 'documents';
    if (!onList && editing()) { documentWanted = true; return Promise.resolve(); }
    documentWanted = false;
    var url = onList ? documentsPath() + '/list' : documentsPath() + '/' + body.dataset.documentId + '/content';
    return fetchFragment(url).then(function (html) {
      var current = $(onList ? '#doc-tree' : '#doc');
      if (!html || !current) { return; }
      if (!onList && editing()) { documentWanted = true; return; }
      var fresh = parseFragment(html);
      if (!fresh) { return; }
      current.replaceWith(fresh);
      localizeTimes(fresh);
    });
  }

  function scheduleDocumentRefresh() {
    clearTimeout(documentTimer);
    documentTimer = setTimeout(refreshDocument, 150);
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
    } else if (page === 'run') {
      if (data.entity_kind === 'run' && String(data.entity_id) === body.dataset.runId) { refreshRun(); }
    } else if (page === 'issue' && data.entity_kind === 'run') {
      refreshRuns();
    } else if (page === 'issue' && data.entity_kind === 'issue' && String(data.entity_id) === body.dataset.issueId) {
      refreshActivity();
      if (typeof data.kind === 'string' && data.kind.indexOf('comment.') === 0) { scheduleCommentsRefresh(); }
    } else if (page === 'initiatives') {
      if (data.entity_kind === 'initiative' || data.entity_kind === 'issue') { scheduleInitiativeRefresh(); }
    } else if (page === 'initiative') {
      if (data.entity_kind === 'initiative' && String(data.entity_id) === body.dataset.initiativeId) {
        if (data.kind === 'initiative.deleted') { location.href = initiativesPath(); return; }
        scheduleInitiativeRefresh();
      } else if (data.entity_kind === 'issue') {
        scheduleInitiativeRefresh();
      }
    } else if (page === 'documents') {
      if (data.entity_kind === 'document') { scheduleDocumentRefresh(); }
    } else if (page === 'document') {
      if (data.entity_kind === 'document' && String(data.entity_id) === body.dataset.documentId) {
        scheduleDocumentRefresh();
      }
    }
  }

  function connect() {
    var live = page === 'board' || page === 'issue' || page === 'run' || page === 'initiatives' || page === 'initiative' ||
      page === 'documents' || page === 'document';
    if (!window.EventSource || !project || !live) { return; }
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
  initOutput();
  syncIssueOutput();
  connect();
}());

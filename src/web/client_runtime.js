(function () {
  'use strict';
  const signals = Object.create(null);
  const signalBindings = Object.create(null);
  let documentId = 'app';
  let revision = '0';
  let hasAuthoritativeDocument = false;

  function refreshSignal(name) {
    const bindings = signalBindings[name];
    if (!bindings) return;
    bindings.forEach(function (el) {
      el.textContent = signals[name];
    });
  }

  function validRevision(value) {
    if (typeof value !== 'string' || !/^(0|[1-9][0-9]*)$/.test(value)) return false;
    try { return BigInt(value) <= 18446744073709551615n; } catch (_) { return false; }
  }

  function wireText(value) {
    if (!value || typeof value !== 'object') return undefined;
    if (value.type === 'string' && typeof value.value === 'string') return value.value;
    if (value.type === 'bool' && typeof value.value === 'boolean') return String(value.value);
    if (value.type === 'i64' && typeof value.value === 'string' && /^-?[0-9]+$/.test(value.value)) return value.value;
    if (value.type === 'null') return '';
    return undefined;
  }

  // Keep the DOM adapter's snapshot checks aligned with UiDocument::validate:
  // unique nonempty node IDs, reachable root, valid child links, no duplicate
  // children, multiple parents or cycles. Unknown valid node kinds are allowed.
  function validUiDocument(doc) {
    const validId = (value) => typeof value === 'string' && value.trim().length > 0;
    if (!doc || doc.protocol !== 'nulang-ui/1' ||
        !validId(doc.document_id) || !validId(doc.root) ||
        !validRevision(doc.revision) || !Array.isArray(doc.nodes)) return false;
    const nodes = new Map();
    for (const node of doc.nodes) {
      if (!node || !validId(node.id) || !validId(node.kind) || nodes.has(node.id) ||
          (node.children !== undefined && !Array.isArray(node.children)) ||
          (node.actions !== undefined && !Array.isArray(node.actions)) ||
          (node.properties !== undefined && (!node.properties || typeof node.properties !== 'object' ||
            Array.isArray(node.properties)))) return false;
      for (const action of node.actions || []) {
        if (!action || !validId(action.event) || !validId(action.action_id) ||
            (action.placement !== 'client' && action.placement !== 'server')) return false;
      }
      nodes.set(node.id, node);
    }
    if (!nodes.has(doc.root)) return false;
    const parents = new Set();
    for (const node of nodes.values()) {
      const local = new Set();
      for (const child of node.children || []) {
        if (!validId(child) || !nodes.has(child) || local.has(child) || parents.has(child))
          return false;
        local.add(child);
        parents.add(child);
      }
    }
    const visited = new Set();
    const stack = [doc.root];
    while (stack.length) {
      const id = stack.pop();
      if (visited.has(id)) return false;
      visited.add(id);
      for (const child of nodes.get(id).children || []) stack.push(child);
    }
    return visited.size === nodes.size;
  }



  // Accept only the protocol operations that this DOM micro-runtime can apply.
  // Validate the entire patch before touching DOM state (all-or-nothing).
  function applyPatch(patch) {
    if (!hasAuthoritativeDocument || !patch || patch.protocol !== 'nulang-ui/1' ||
        patch.document_id !== documentId ||
        !validRevision(patch.base_revision) || !validRevision(patch.revision) ||
        patch.base_revision !== revision || BigInt(patch.revision) <= BigInt(revision) ||
        !Array.isArray(patch.operations)) return false;
    const changes = new Map();
    for (const op of patch.operations) {
      if (!op || op.op !== 'set_property' || op.name !== 'value' || typeof op.node_id !== 'string') return false;
      const match = /^(?:signal|read):[0-9]+:(.+)$/.exec(op.node_id);
      if (!match || !Object.prototype.hasOwnProperty.call(signalBindings, match[1])) return false;
      const value = wireText(op.value);
      if (value === undefined) return false;
      // A signal patch may contain both the canonical signal and its read nodes.
      if (changes.has(match[1]) && changes.get(match[1]) !== value) return false;
      changes.set(match[1], value);
    }
    changes.forEach(function (value, name) {
      signals[name] = value;
      refreshSignal(name);
    });
    revision = patch.revision;
    return true;
  }

  function applySnapshot(snapshot) {
    if (!validUiDocument(snapshot) ||
        (hasAuthoritativeDocument && (snapshot.document_id !== documentId ||
          BigInt(snapshot.revision) < BigInt(revision)))) return false;

    // Validate every recognized signal before mutating the DOM or revision.
    const changes = new Map();
    for (const node of snapshot.nodes) {
      if (node.kind !== 'signal') continue;
      const named = node.properties && node.properties.name;
      if (!named || named.type !== 'string' ||
          !Object.prototype.hasOwnProperty.call(signalBindings, named.value)) continue;
      // A compiler-created signal starts at WireValue::Null: keep the
      // server-rendered DOM text until an actual signal value is published.
      if (!node.properties.value || node.properties.value.type === 'null') continue;
      const value = wireText(node.properties.value);
      if (value === undefined) return false;
      changes.set(named.value, value);
    }
    changes.forEach(function (value, name) {
      signals[name] = value;
      refreshSignal(name);
    });
    documentId = snapshot.document_id;
    revision = snapshot.revision;
    hasAuthoritativeDocument = true;
    return true;
  }

  function receiveUiMessage(message) {
    if (!message || message.protocol !== 'nulang-ui-msg/1') return false;
    if (message.type === 'patch') return applyPatch(message.patch);
    if (message.type === 'snapshot') return applySnapshot(message.document);
    return false;
  }

  function runClientAction(handler) {
    if (window.nulangActions && typeof window.nulangActions[handler] === 'function') {
      window.nulangActions[handler]();
    } else {
      console.warn('nulang: missing client action handler', handler);
    }
  }

  function actionMessageId(prefix) {
    if (globalThis.crypto && typeof globalThis.crypto.randomUUID === 'function') {
      return prefix + ':' + globalThis.crypto.randomUUID();
    }
    return prefix + ':' + Date.now() + ':' + Math.random().toString(16).slice(2);
  }

  function wireFormPayload(body) {
    const fields = {};
    body.forEach(function (value, key) {
      if (typeof value === 'string') fields[key] = { type: 'string', value: value };
    });
    return { type: 'object', value: fields };
  }

  function createActionMessage(handler, placement, body) {
    return {
      type: 'invoke_action',
      protocol: 'nulang-ui-msg/1',
      request: {
        document_id: documentId,
        revision: revision,
        action_id: handler,
        placement: placement,
        correlation_id: actionMessageId('corr'),
        idempotency_key: actionMessageId('idem'),
        payload: wireFormPayload(body)
      }
    };
  }

  function formBody(form) {
    const body = new URLSearchParams();
    if (!form) return body;
    new FormData(form).forEach(function (value, key) {
      if (typeof value === 'string') body.append(key, value);
    });
    return body;
  }

  async function runServerAction(handler, el) {
    const form = el.closest('form');
    const body = formBody(form);
    const message = createActionMessage(handler, 'server', body);
    body.append('__nulang_action', handler);
    body.append('__nulang_ui_message', JSON.stringify(message));
    const response = await fetch(window.location.href, { method: 'POST', body: body });
    if (!response.ok) throw new Error('server action failed: ' + response.status);
    const type = response.headers && response.headers.get('content-type') || '';
    if (/^application\/(?:vnd\.nulang-ui\+json|json)(?:\s*;|$)/i.test(type)) {
      const next = await response.json();
      if (receiveUiMessage(next)) return;
    }
    // Existing server endpoints return HTML: retain compatibility until the
    // server can emit a verified patch or snapshot for this action.
    window.location.reload();
  }

  function hydrate() {
    document.querySelectorAll('[data-signal]').forEach(function (el) {
      const name = el.dataset.signal;
      if (!Object.prototype.hasOwnProperty.call(signals, name)) signals[name] = el.textContent;
      if (!signalBindings[name]) signalBindings[name] = [];
      signalBindings[name].push(el);
    });

    // A page may include a validated, server-produced initial UiDocument as a
    // JSON snapshot. Without that handshake, reject canonical patches rather
    // than silently assuming the authoritative revision is zero.
    const bootstrap = document.getElementById &&
      document.getElementById('nulang-ui-bootstrap');
    if (bootstrap) {
      try {
        if (!receiveUiMessage(JSON.parse(bootstrap.textContent))) {
          console.warn('nulang: rejected UI bootstrap snapshot');
        }
      } catch (_) {
        console.warn('nulang: malformed UI bootstrap snapshot');
      }
    }

    document.querySelectorAll('[data-action]').forEach(function (el) {
      const handler = el.dataset.action;
      const placement = el.dataset.actionPlacement;
      const listener = function (e) {
        e.preventDefault();
        if (placement === 'server') {
          runServerAction(handler, el).catch(function (err) {
            console.error('nulang: server action failed', err);
          });
        } else {
          runClientAction(handler);
        }
      };
      // Forms receive a click followed by submit. Register once, on submit.
      el.addEventListener(el.tagName === 'FORM' ? 'submit' : 'click', listener);
    });
  }

  window.nulang = {
    signal: function (name) {
      return {
        get: function () { return signals[name]; },
        set: function (value) { signals[name] = value; refreshSignal(name); }
      };
    },
    receiveUiMessage: receiveUiMessage,
    uiRevision: function () { return revision; }
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', hydrate);
  } else {
    hydrate();
  }
})();

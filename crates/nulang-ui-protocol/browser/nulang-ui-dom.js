/* Nulang semantic UI DOM host (nulang-ui/1). Dependency-free and opt-in.
 * Every server document is untrusted until validated; DOM is always created
 * through DOM APIs, never through HTML string interpolation.
 */
(function (global) {
  "use strict";
  const tags = new Set([
    "a", "article", "aside", "button", "div", "fieldset", "footer",
    "form", "h1", "h2", "h3", "header", "input", "label", "li",
    "main", "nav", "ol", "option", "p", "section", "select", "span",
    "strong", "table", "tbody", "td", "textarea", "th", "thead", "tr", "ul"
  ]);
  const aliases = {column: "div", row: "div"};
  const stringProps = new Set([
    "class", "id", "title", "role", "name", "type", "placeholder",
    "autocomplete", "alt", "href", "src", "target", "rel", "for"
  ]);
  const booleanProps = new Set(["disabled", "required", "readonly", "multiple"]);
  const editable = new Set(["value", "checked"]);

  function fail(message) { throw new Error("nulang-ui: " + message); }
  function revision(raw) {
    if (typeof raw !== "string" || !/^(0|[1-9][0-9]*)$/.test(raw)) fail("invalid revision");
    const number = BigInt(raw);
    if (number > 18446744073709551615n) fail("revision outside u64");
    return number;
  }
  function id(value) {
    if (typeof value !== "string" || !value.trim()) fail("empty node identifier");
    return value;
  }
  function wire(value) {
    if (!value || typeof value !== "object") fail("invalid property value");
    switch (value.type) {
      case "string": if (typeof value.value === "string") return value.value; break;
      case "bool": if (typeof value.value === "boolean") return value.value; break;
      case "i64": if (typeof value.value === "string" && /^-?\d+$/.test(value.value)) return value.value; break;
      case "null": return null;
    }
    fail("unsupported property value type");
  }
  function safeURL(value, property) {
    if (typeof value !== "string" || /[\u0000-\u0020\\]/.test(value)) return false;
    if (value.startsWith("//")) return false;
    if (value.startsWith("/") || value.startsWith("#") || value.startsWith("?")) return true;
    return property === "href"
      ? /^(https?:\/\/|mailto:|tel:)/i.test(value)
      : /^https?:\/\//i.test(value);
  }
  function tagFor(kind) {
    if (kind === "text") return "#text";
    const tag = aliases[kind] || kind;
    if (!tags.has(tag)) fail("unsupported node kind " + kind);
    return tag;
  }
  function validateProperty(name, value, kind) {
    const raw = wire(value);
    if (name === "text") {
      if (typeof raw !== "string") fail("text property must be a string");
    } else if (name === "value") {
      if (typeof raw !== "string") fail("input value must be a string");
    } else if (name === "checked") {
      if (typeof raw !== "boolean") fail("checked must be a boolean");
    } else if (name === "href" || name === "src") {
      if (!safeURL(raw, name)) fail("unsafe " + name);
    } else if (booleanProps.has(name)) {
      if (typeof raw !== "boolean") fail("invalid boolean attribute");
    } else if (stringProps.has(name) || /^aria-[a-z0-9-]+$/.test(name)) {
      if (typeof raw !== "string") fail("invalid string attribute");
    } else {
      fail("unsupported property " + name);
    }
    if (kind === "text" && name !== "text") fail("text nodes accept only text");
  }

  function checkedDocument(document) {
    if (!document || document.protocol !== "nulang-ui/1") fail("unsupported document");
    id(document.document_id); id(document.root); revision(document.revision);
    if (!Array.isArray(document.nodes) || document.nodes.length > 10000) fail("invalid node count");
    const map = new Map();
    for (const node of document.nodes) {
      id(node.id);
      if (node.id.length > 1024) fail("node id too long");
      tagFor(node.kind);
      if (map.has(node.id)) fail("duplicate node " + node.id);
      if (!Array.isArray(node.children)) fail("missing children");
      if (node.kind === "text" && node.children.length) fail("text node cannot have children");
      if (!node.properties || typeof node.properties !== "object" || Array.isArray(node.properties)) fail("invalid properties");
      for (const [name, value] of Object.entries(node.properties)) validateProperty(name, value, node.kind);
      map.set(node.id, node);
    }
    if (!map.has(document.root)) fail("missing root");
    const parents = new Set();
    for (const node of map.values()) {
      const local = new Set();
      for (const child of node.children) {
        id(child);
        if (!map.has(child) || local.has(child) || parents.has(child)) fail("invalid child identity");
        local.add(child); parents.add(child);
      }
    }
    const seen = new Set();
    const todo = [document.root];
    while (todo.length) {
      const current = todo.pop();
      if (seen.has(current)) fail("cyclic document");
      seen.add(current);
      todo.push(...map.get(current).children);
    }
    if (seen.size !== map.size) fail("unreachable node");
    return map;
  }

  function checkedPatch(patch, document) {
    if (!patch || patch.protocol !== "nulang-ui/1") fail("unsupported patch");
    if (patch.document_id !== document.document_id) fail("wrong document");
    const base = revision(patch.base_revision), next = revision(patch.revision);
    if (next <= base) fail("nonincreasing patch");
    if (!Array.isArray(patch.operations)) fail("invalid operations");
    const acks = patch.acknowledged_inputs || [];
    if (!Array.isArray(acks)) fail("invalid acknowledgements");
    const unique = new Set();
    for (const ack of acks) {
      id(ack.node_id);
      if (!editable.has(ack.property)) fail("unsupported acknowledgement property");
      revision(ack.sequence);
      const key = JSON.stringify([ack.node_id, ack.property]);
      if (unique.has(key)) fail("duplicate acknowledgement");
      unique.add(key);
    }
    return {base, next, acks};
  }

  function patchedDocument(document, patch) {
    const next = structuredClone(document);
    const nodes = new Map(next.nodes.map(node => [node.id, node]));
    for (const operation of patch.operations) {
      if (!operation || typeof operation.op !== "string") fail("invalid patch operation");
      const node = nodes.get(operation.node_id);
      switch (operation.op) {
        case "upsert_node":
          if (!operation.node || typeof operation.node.id !== "string") fail("invalid upsert");
          nodes.set(operation.node.id, structuredClone(operation.node));
          break;
        case "remove_node":
          if (!nodes.delete(operation.node_id)) fail("unknown node");
          break;
        case "set_root": next.root = operation.node_id; break;
        case "set_property":
          if (!node) fail("unknown node");
          if (typeof operation.name !== "string") fail("invalid property name");
          validateProperty(operation.name, operation.value, node.kind);
          node.properties[operation.name] = structuredClone(operation.value);
          break;
        case "remove_property":
          if (!node) fail("unknown node");
          if (typeof operation.name !== "string") fail("invalid property name");
          delete node.properties[operation.name];
          break;
        case "replace_children":
          if (!node || !Array.isArray(operation.children)) fail("invalid children");
          node.children = [...operation.children];
          break;
        case "replace_actions":
          if (!node || !Array.isArray(operation.actions)) fail("invalid actions");
          node.actions = structuredClone(operation.actions);
          break;
        default: fail("unsupported patch operation");
      }
    }
    next.nodes = [...nodes.values()];
    next.revision = patch.revision;
    checkedDocument(next);
    return next;
  }

  // The SSR bridge derives the same scoped key IDs as the Rust KeyedNode
  // builder. Text-only leaves become a "text" property; mixed text/element
  // children are deliberately rejected until compiler-owned text ranges exist.
  function snapshotFromDom(container, options) {
    const root = container.firstElementChild;
    if (!root || [...container.childNodes].some(node =>
      node !== root && (node.nodeType !== 3 || node.textContent.trim()))) {
      fail("SSR mount must have one root element");
    }
    if (root.hasAttribute("data-nulang-key")) fail("SSR root key is unsupported");
    if (!options || typeof options.documentId !== "string") fail("missing document id");
    id(options.documentId);
    revision(options.revision);
    const nodes = [];
    const elements = new Map();
    const encoder = new TextEncoder();
    function visit(element, nodeId, depth) {
      if (depth > 64 || nodeId.length > 1024 || nodes.length >= 10000) {
        fail("SSR tree budget exceeded");
      }
      const kind = element.tagName.toLowerCase();
      tagFor(kind);
      const properties = {};
      for (const attr of element.attributes) {
        if (["data-nulang-key", "data-nulang-node-id", "data-signal",
             "data-action", "data-action-placement"].includes(attr.name)) continue;
        if (attr.name === "value" || attr.name === "checked") continue;
        const value = booleanProps.has(attr.name) ? {type:"bool",value:true} : {type:"string",value:attr.value};
        validateProperty(attr.name, value, kind);
        properties[attr.name] = value;
      }
      if (["input", "textarea", "select"].includes(kind)) {
        if (kind !== "select" || !element.children.length) {
          properties.value = {type:"string",value:element.value};
        }
        if (kind === "input" && ["checkbox", "radio"].includes(element.type)) {
          properties.checked = {type:"bool",value:element.checked};
        }
      }
      const children = [];
      const elementChildren = [...element.children];
      if (elementChildren.length) {
        if ([...element.childNodes].some(child => child.nodeType === 3 && child.textContent.trim())) {
          fail("mixed SSR text needs compiled text ranges");
        }
      } else if (!["input", "textarea", "select"].includes(kind) && element.textContent) {
        properties.text = {type:"string",value:element.textContent};
      }
      const siblingKeys = new Set();
      elementChildren.forEach((child,index) => {
        const key = child.getAttribute("data-nulang-key");
        let segment;
        if (key === null) {
          segment = "p" + index;
        } else {
          const bytes = encoder.encode(key);
          if (!key.trim() || bytes.length > 256 || /[\x00-\x1f\x7f]/.test(key) || siblingKeys.has(key)) {
            fail("invalid or duplicate sibling key");
          }
          siblingKeys.add(key);
          segment = "k" + [...bytes].map(byte=>byte.toString(16).padStart(2,"0")).join("");
        }
        children.push(nodeId + "/" + segment);
      });
      nodes.push({id:nodeId,kind,properties,children});
      elements.set(nodeId,element);
      elementChildren.forEach((child,index)=>visit(child,children[index],depth+1));
    }
    visit(root,"root",0);
    const document = {
      protocol:"nulang-ui/1", document_id:options.documentId,
      revision:options.revision, root:"root", nodes
    };
    checkedDocument(document);
    return {document,elements};
  }

  class DomHost {
    constructor(container, snapshot, adoptedElements = null) {
      if (!container || !container.ownerDocument) fail("invalid mount");
      this.container = container;
      this.document = structuredClone(snapshot);
      checkedDocument(this.document);
      this.elements = adoptedElements ? new Map(adoptedElements) : new Map();
      this.renderedProps = new Map();
      if (adoptedElements) {
        for (const node of this.document.nodes) {
          const element = this.elements.get(node.id);
          if (!element) fail("incomplete SSR adoption");
          this.renderedProps.set(node.id,structuredClone(node.properties));
          element.setAttribute("data-nulang-node-id",node.id);
          this.bindInput(node.id,element);
        }
      }
      this.pending = new Map();
      this.sequences = new Map();
      this.composing = new Set();
      this.queued = [];
      this.onInput = null;
      this.render();
    }

    element(nodeId) { return this.elements.get(nodeId); }

    recordInput(nodeId, property, value) {
      if (!editable.has(property)) fail("unsupported local input");
      const node = this.document.nodes.find(item => item.id === nodeId);
      if (!node) fail("unknown node");
      if (!["input", "textarea", "select"].includes(node.kind)) fail("not a form control");
      const key = JSON.stringify([nodeId, property]);
      const sequence = (this.sequences.get(key) || 0n) + 1n;
      if (sequence > 18446744073709551615n) fail("input sequence exhausted");
      this.sequences.set(key, sequence);
      const encoded = {type: typeof value === "boolean" ? "bool" : "string", value};
      validateProperty(property, encoded, node.kind);
      node.properties[property] = encoded;
      this.pending.set(key, {nodeId, property, sequence, value:encoded});
      const detail = {node_id:nodeId, property, sequence:sequence.toString(), value:encoded};
      if (typeof this.onInput === "function") this.onInput(detail);
      return detail;
    }

    applyPatch(patch) {
      const {base, next, acks} = checkedPatch(patch, this.document);
      if (next <= revision(this.document.revision)) return "stale";
      if (this.composing.size) {
        if (this.queued.length >= 32) fail("composition patch queue exceeded; resync required");
        this.queued.push(structuredClone(patch));
        return "deferred";
      }
      if (base !== revision(this.document.revision)) fail("revision gap; resync required");
      const newDocument = patchedDocument(this.document, patch);
      const pending = new Map(this.pending);
      const sequences = new Map(this.sequences);
      for (const ack of acks) {
        const key = JSON.stringify([ack.node_id, ack.property]);
        const edit = pending.get(key);
        if (edit && revision(ack.sequence) >= edit.sequence) pending.delete(key);
      }
      const updated = new Map(newDocument.nodes.map(node => [node.id, node]));
      const previous = new Map(this.document.nodes.map(node => [node.id, node]));
      for (const [key, edit] of pending) {
        const node = updated.get(edit.nodeId);
        if (!node || !previous.has(edit.nodeId) || previous.get(edit.nodeId).kind !== node.kind) {
          pending.delete(key);
        } else {
          node.properties[edit.property] = structuredClone(edit.value);
        }
      }
      for (const [key] of sequences) {
        const [nodeId] = JSON.parse(key);
        if (!updated.has(nodeId)) sequences.delete(key);
      }
      checkedDocument(newDocument);
      this.document = newDocument;
      this.pending = pending;
      this.sequences = sequences;
      this.render();
      return "applied";
    }

    // Snapshot recovery is explicit. Never silently discard drafts or IME
    // composition; reconnect callers must make that product-level decision.
    resync(snapshot, options = {}) {
      const candidate = structuredClone(snapshot);
      checkedDocument(candidate);
      if (candidate.document_id !== this.document.document_id) fail("cross-document snapshot");
      if (revision(candidate.revision) < revision(this.document.revision)) fail("stale snapshot");
      if ((this.pending.size || this.composing.size || this.queued.length) && options.discardPending !== true) {
        fail("snapshot would discard local input; explicit discardPending required");
      }
      this.document = candidate;
      this.pending.clear();
      this.composing.clear();
      this.queued = [];
      // Keep per-input sequence high-water marks across snapshots to avoid
      // reusing a sequence while the same session still has outstanding acks.
      this.render();
      return "resynced";
    }

    applyMessage(message) {
      if (!message || message.protocol !== "nulang-ui-msg/1") fail("unsupported message");
      if (message.type === "patch") return this.applyPatch(message.patch);
      if (message.type === "snapshot") return this.resync(message.document);
      fail("unsupported runtime message");
    }

    bindInput(id, element) {
      if (!["INPUT", "TEXTAREA", "SELECT"].includes(element.tagName)) return;
      const emit = () => {
        if (element.type === "checkbox" || element.type === "radio") {
          this.recordInput(id, "checked", element.checked);
        } else {
          this.recordInput(id, "value", element.value);
        }
      };
      element.addEventListener("input", emit);
      element.addEventListener("compositionstart", () => this.composing.add(id));
      element.addEventListener("compositionend", () => {
        this.composing.delete(id);
        if (this.composing.size) return;
        const queued = this.queued.splice(0);
        for (const patch of queued) this.applyPatch(patch);
      });
    }

    render() {
      const spec = checkedDocument(this.document);
      const dom = this.container.ownerDocument;
      const focused = dom.activeElement;
      let selection;
      if (focused && typeof focused.selectionStart === "number") {
        selection = {start:focused.selectionStart,end:focused.selectionEnd,direction:focused.selectionDirection};
      }
      const seen = new Set();
      const visit = id => {
        const node = spec.get(id);
        let element = this.elements.get(id);
        const tag = tagFor(node.kind);
        if (!element || (tag === "#text" ? element.nodeType !== 3 : element.nodeName.toLowerCase() !== tag)) {
          element = tag === "#text" ? dom.createTextNode("") : dom.createElement(tag);
          if (tag !== "#text") {
            element.setAttribute("data-nulang-node-id", id);
            this.bindInput(id, element);
          }
          this.elements.set(id, element);
          this.renderedProps.delete(id);
        }
        seen.add(id);
        const old = this.renderedProps.get(id) || {};
        const next = node.properties;
        if (tag === "#text") {
          const value = next.text ? wire(next.text) : "";
          if (element.nodeValue !== value) element.nodeValue = value;
        } else {
          for (const key of Object.keys(old)) {
            if (!(key in next)) this.unsetProperty(element, key);
          }
          for (const [key, value] of Object.entries(next)) {
            if (JSON.stringify(old[key]) !== JSON.stringify(value)) this.setProperty(element, key, value, node.children.length);
          }
          if (node.children.length) {
            const children = node.children.map(visit);
            let anchor = null;
            for (let index=children.length-1; index>=0; index--) {
              const child = children[index];
              if (child.parentNode !== element || child.nextSibling !== anchor) {
                element.insertBefore(child, anchor);
              }
              anchor=child;
            }
            const keep = new Set(children);
            for (const child of [...element.childNodes]) if (!keep.has(child)) element.removeChild(child);
          } else if (!Object.hasOwn(next,"text")) {
            if (element.childNodes.length) element.replaceChildren();
          }
        }
        this.renderedProps.set(id, structuredClone(next));
        return element;
      };
      const root = visit(this.document.root);
      if (this.container.firstChild !== root || this.container.childNodes.length !== 1) {
        this.container.replaceChildren(root);
      }
      for (const [id, element] of this.elements) {
        if (!seen.has(id)) {
          element.remove();
          this.elements.delete(id);
          this.renderedProps.delete(id);
        }
      }
      if (focused && focused.isConnected && dom.activeElement !== focused) {
        focused.focus({preventScroll:true});
      }
      if (selection && focused && focused.isConnected) {
        try { focused.setSelectionRange(selection.start,selection.end,selection.direction); } catch (_) {}
      }
    }

    unsetProperty(element, key) {
      if (key === "text") element.textContent="";
      else if (key === "value") element.value="";
      else if (key === "checked") element.checked=false;
      else if (booleanProps.has(key)) element.removeAttribute(key);
      else element.removeAttribute(key === "class" ? "class" : key);
    }
    setProperty(element, key, value, hasChildren) {
      const raw = wire(value);
      if (key === "text") {
        if (!hasChildren && element.textContent !== raw) element.textContent=raw;
      } else if (key === "value") {
        if (element.value !== raw) element.value=raw;
      } else if (key === "checked") {
        element.checked=raw;
      } else if (booleanProps.has(key)) {
        if (raw) element.setAttribute(key,""); else element.removeAttribute(key);
      } else {
        element.setAttribute(key,raw);
      }
    }
  }

  global.nulangUiDom = Object.freeze({
    createHost:(container,snapshot)=>new DomHost(container,snapshot),
    createHostFromDom:(container,options)=>{
      const {document,elements} = snapshotFromDom(container,options);
      return new DomHost(container,document,elements);
    }
  });
})(globalThis);

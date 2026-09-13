//! The standard surface a program finds before it runs, as a program.
//!
//! This is the façade layer: the JavaScript-visible API a profile puts in
//! front of a program, held here as source and compiled by whichever host
//! wants it, through the same front end and verifier as any other program.
//! It is not engine code and takes no privilege. Every member is either a
//! pure function of its arguments and the heap, or it goes through a
//! capability the deployment granted — `console` writes through the output
//! the host installed, timers through the clock it was given, and nothing
//! else reaches outside at all.
//!
//! Keeping it here rather than in `realm.rs` is the whole point: a build
//! that wants none of it links none of it, and a member that needs a
//! capability cannot acquire one behind the program's back, because it
//! reaches the outside through the same admitted bindings a program does.

/// The façade's source. A host compiles it once and runs it in the realm
/// before the program, so what a program sees is already there.
pub const SOURCE: &[u8] = br##"// The standard surface a program finds before it runs, as a program itself.
//
// Everything here is a pure function of its arguments and the heap, or it
// goes through a capability the deployment granted. Nothing reaches the
// outside on its own: `console` writes through the output the host installed,
// and every other member is arithmetic over values.
(function () {
  "use strict";
  const G = globalThis;

  // ---- console ------------------------------------------------------
  // Output is authority: `print` exists because the host installed it, and
  // console is the shape a person expects over the top of it.
  function render(value, depth) {
    if (typeof value === "string") return depth === 0 ? value : '"' + value + '"';
    if (value === null) return "null";
    if (value === undefined) return "undefined";
    if (typeof value === "bigint") return String(value) + "n";
    if (typeof value === "symbol") return String(value);
    if (typeof value === "function") return "[Function]";
    if (typeof value !== "object") return String(value);
    if (depth > 2) return Array.isArray(value) ? "[Array]" : "[Object]";
    if (Array.isArray(value)) {
      const parts = [];
      for (let i = 0; i < value.length; i++) parts.push(render(value[i], depth + 1));
      return "[" + parts.join(", ") + "]";
    }
    const keys = Object.keys(value);
    const parts = [];
    for (let i = 0; i < keys.length; i++) {
      parts.push(keys[i] + ": " + render(value[keys[i]], depth + 1));
    }
    return "{" + parts.join(", ") + "}";
  }

  function line(args) {
    const parts = [];
    for (let i = 0; i < args.length; i++) parts.push(render(args[i], 0));
    return parts.join(" ");
  }

  if (typeof print === "function") {
    const write = print;
    G.console = {
      log: function () { write(line(arguments)); },
      info: function () { write(line(arguments)); },
      warn: function () { write(line(arguments)); },
      error: function () { write(line(arguments)); },
      debug: function () { write(line(arguments)); },
    };
  }

  // ---- text encoding ------------------------------------------------
  G.TextEncoder = class TextEncoder {
    get encoding() { return "utf-8"; }
    encode(input) {
      const text = input === undefined ? "" : String(input);
      const bytes = [];
      for (let i = 0; i < text.length; i++) {
        let point = text.charCodeAt(i);
        if (point >= 0xd800 && point <= 0xdbff && i + 1 < text.length) {
          const low = text.charCodeAt(i + 1);
          if (low >= 0xdc00 && low <= 0xdfff) {
            point = 0x10000 + ((point - 0xd800) << 10) + (low - 0xdc00);
            i++;
          }
        }
        if (point < 0x80) bytes.push(point);
        else if (point < 0x800) {
          bytes.push(0xc0 | (point >> 6), 0x80 | (point & 0x3f));
        } else if (point < 0x10000) {
          bytes.push(0xe0 | (point >> 12), 0x80 | ((point >> 6) & 0x3f), 0x80 | (point & 0x3f));
        } else {
          bytes.push(
            0xf0 | (point >> 18),
            0x80 | ((point >> 12) & 0x3f),
            0x80 | ((point >> 6) & 0x3f),
            0x80 | (point & 0x3f)
          );
        }
      }
      return Uint8Array.from(bytes);
    }
  };

  G.TextDecoder = class TextDecoder {
    get encoding() { return "utf-8"; }
    decode(input) {
      if (input === undefined) return "";
      const bytes = input;
      let out = "";
      let i = 0;
      while (i < bytes.length) {
        const first = bytes[i];
        let point;
        let width;
        if (first < 0x80) { point = first; width = 1; }
        else if ((first & 0xe0) === 0xc0) { point = first & 0x1f; width = 2; }
        else if ((first & 0xf0) === 0xe0) { point = first & 0x0f; width = 3; }
        else { point = first & 0x07; width = 4; }
        if (i + width > bytes.length) { out += "\uFFFD"; break; }
        for (let k = 1; k < width; k++) point = (point << 6) | (bytes[i + k] & 0x3f);
        i += width;
        if (point > 0xffff) {
          const offset = point - 0x10000;
          out += String.fromCharCode(0xd800 + (offset >> 10), 0xdc00 + (offset & 0x3ff));
        } else {
          out += String.fromCharCode(point);
        }
      }
      return out;
    }
  };

  // ---- base64 -------------------------------------------------------
  const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  G.btoa = function btoa(input) {
    const text = String(input);
    let out = "";
    for (let i = 0; i < text.length; i += 3) {
      const a = text.charCodeAt(i);
      const b = i + 1 < text.length ? text.charCodeAt(i + 1) : 0;
      const c = i + 2 < text.length ? text.charCodeAt(i + 2) : 0;
      if (a > 255 || b > 255 || c > 255) throw new TypeError("btoa: not latin-1");
      out += ALPHABET[a >> 2];
      out += ALPHABET[((a & 3) << 4) | (b >> 4)];
      out += i + 1 < text.length ? ALPHABET[((b & 15) << 2) | (c >> 6)] : "=";
      out += i + 2 < text.length ? ALPHABET[c & 63] : "=";
    }
    return out;
  };
  G.atob = function atob(input) {
    const text = String(input).replace("=", "").replace("=", "");
    let out = "";
    let bits = 0;
    let held = 0;
    for (let i = 0; i < text.length; i++) {
      const value = ALPHABET.indexOf(text[i]);
      if (value < 0) throw new TypeError("atob: not base64");
      held = (held << 6) | value;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out += String.fromCharCode((held >> bits) & 0xff);
      }
    }
    return out;
  };

  // ---- URL ----------------------------------------------------------
  // Pure string work, so no capability at all: parsing a name is not
  // reaching anything.
  const SAFE = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
  function encodeComponent(text) {
    let out = "";
    const bytes = new G.TextEncoder().encode(String(text));
    for (let i = 0; i < bytes.length; i++) {
      const ch = String.fromCharCode(bytes[i]);
      if (SAFE.indexOf(ch) >= 0) out += ch;
      else if (ch === " ") out += "+";
      else out += "%" + (bytes[i] < 16 ? "0" : "") + bytes[i].toString(16).toUpperCase();
    }
    return out;
  }
  function decodeComponent(text) {
    const source = String(text);
    const bytes = [];
    for (let i = 0; i < source.length; i++) {
      const ch = source[i];
      if (ch === "+") bytes.push(32);
      else if (ch === "%" && i + 2 < source.length) {
        bytes.push(parseInt(source.substring(i + 1, i + 3), 16));
        i += 2;
      } else bytes.push(source.charCodeAt(i));
    }
    return new G.TextDecoder().decode(Uint8Array.from(bytes));
  }

  G.URLSearchParams = class URLSearchParams {
    constructor(init) {
      this._pairs = [];
      if (typeof init === "string") {
        const text = init[0] === "?" ? init.substring(1) : init;
        if (text.length > 0) {
          const parts = text.split("&");
          for (let i = 0; i < parts.length; i++) {
            if (parts[i].length === 0) continue;
            const at = parts[i].indexOf("=");
            if (at < 0) this._pairs.push([decodeComponent(parts[i]), ""]);
            else {
              this._pairs.push([
                decodeComponent(parts[i].substring(0, at)),
                decodeComponent(parts[i].substring(at + 1)),
              ]);
            }
          }
        }
      }
    }
    get(name) {
      const key = String(name);
      for (let i = 0; i < this._pairs.length; i++) {
        if (this._pairs[i][0] === key) return this._pairs[i][1];
      }
      return null;
    }
    getAll(name) {
      const key = String(name);
      const out = [];
      for (let i = 0; i < this._pairs.length; i++) {
        if (this._pairs[i][0] === key) out.push(this._pairs[i][1]);
      }
      return out;
    }
    has(name) { return this.get(name) !== null; }
    set(name, value) {
      const key = String(name);
      for (let i = 0; i < this._pairs.length; i++) {
        if (this._pairs[i][0] === key) { this._pairs[i][1] = String(value); return; }
      }
      this._pairs.push([key, String(value)]);
    }
    append(name, value) { this._pairs.push([String(name), String(value)]); }
    delete(name) {
      const key = String(name);
      const kept = [];
      for (let i = 0; i < this._pairs.length; i++) {
        if (this._pairs[i][0] !== key) kept.push(this._pairs[i]);
      }
      this._pairs = kept;
    }
    forEach(fn) {
      for (let i = 0; i < this._pairs.length; i++) fn(this._pairs[i][1], this._pairs[i][0], this);
    }
    toString() {
      const parts = [];
      for (let i = 0; i < this._pairs.length; i++) {
        parts.push(encodeComponent(this._pairs[i][0]) + "=" + encodeComponent(this._pairs[i][1]));
      }
      return parts.join("&");
    }
  };

  G.URL = class URL {
    constructor(input, base) {
      let text = String(input);
      if (base !== undefined && text.indexOf("://") < 0) {
        const parent = new G.URL(base);
        if (text[0] === "/") text = parent.origin + text;
        else {
          const cut = parent.pathname.lastIndexOf("/");
          text = parent.origin + parent.pathname.substring(0, cut + 1) + text;
        }
      }
      const scheme = text.indexOf("://");
      if (scheme < 0) throw new TypeError("Invalid URL");
      this.protocol = text.substring(0, scheme) + ":";
      let rest = text.substring(scheme + 3);
      let fragment = "";
      const hash = rest.indexOf("#");
      if (hash >= 0) { fragment = rest.substring(hash); rest = rest.substring(0, hash); }
      let query = "";
      const mark = rest.indexOf("?");
      if (mark >= 0) { query = rest.substring(mark); rest = rest.substring(0, mark); }
      const slash = rest.indexOf("/");
      const authority = slash < 0 ? rest : rest.substring(0, slash);
      this.pathname = normalise(slash < 0 ? "/" : rest.substring(slash));
      const colon = authority.lastIndexOf(":");
      if (colon > 0) {
        this.hostname = authority.substring(0, colon);
        this.port = authority.substring(colon + 1);
      } else {
        this.hostname = authority;
        this.port = "";
      }
      this.host = authority;
      this.hash = fragment;
      this.search = query;
      this.searchParams = new G.URLSearchParams(query);
      this.origin = this.protocol + "//" + authority;
    }
    toString() {
      const query = this.searchParams.toString();
      return this.origin + this.pathname + (query.length > 0 ? "?" + query : "") + this.hash;
    }
    toJSON() { return this.toString(); }
  };
  // A path with dot segments resolved, which is what a base-relative name
  // needs before anything compares two of them.
  function normalise(path) {
    const parts = String(path).split("/");
    const kept = [];
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      if (part === ".") continue;
      if (part === "..") { if (kept.length > 1) kept.pop(); continue; }
      kept.push(part);
    }
    const joined = kept.join("/");
    return joined.length === 0 ? "/" : joined;
  }

  // ---- events -------------------------------------------------------
  G.Event = class Event {
    constructor(type, options) {
      this.type = String(type);
      this.defaultPrevented = false;
      this.cancelable = options ? !!options.cancelable : false;
      this.target = null;
    }
    preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
  };

  G.EventTarget = class EventTarget {
    constructor() { this._listeners = {}; }
    addEventListener(type, listener) {
      const key = String(type);
      if (!this._listeners[key]) this._listeners[key] = [];
      if (this._listeners[key].indexOf(listener) < 0) this._listeners[key].push(listener);
    }
    removeEventListener(type, listener) {
      const held = this._listeners[String(type)];
      if (!held) return;
      const at = held.indexOf(listener);
      if (at >= 0) held.splice(at, 1);
    }
    dispatchEvent(event) {
      const held = this._listeners[event.type];
      event.target = this;
      if (held) {
        for (let i = 0; i < held.length; i++) {
          const listener = held[i];
          if (typeof listener === "function") listener.call(this, event);
          else if (listener && typeof listener.handleEvent === "function") {
            listener.handleEvent(event);
          }
        }
      }
      return !event.defaultPrevented;
    }
  };

  G.AbortSignal = class AbortSignal extends G.EventTarget {
    constructor() { super(); this.aborted = false; this.reason = undefined; }
    throwIfAborted() { if (this.aborted) throw this.reason; }
  };

  G.AbortController = class AbortController {
    constructor() { this.signal = new G.AbortSignal(); }
    abort(reason) {
      if (this.signal.aborted) return;
      this.signal.aborted = true;
      this.signal.reason = reason === undefined ? new Error("Aborted") : reason;
      this.signal.dispatchEvent(new G.Event("abort"));
    }
  };

  // ---- microtasks ---------------------------------------------------
  G.queueMicrotask = function queueMicrotask(callback) {
    if (typeof callback !== "function") throw new TypeError("queueMicrotask: not a function");
    Promise.resolve().then(callback);
  };


  // ---- timers -------------------------------------------------------
  // A timer is a capability: the deployment's clock holds the wait, and the
  // callback runs as an ordinary job when it answers. Without that grant
  // there are no timers at all, rather than timers that never fire.
  if (typeof clock === "object" && clock && typeof clock.sleep === "function") {
    const pending = new Map();
    let next = 1;
    G.setTimeout = function setTimeout(callback, delay) {
      if (typeof callback !== "function") throw new TypeError("setTimeout: not a function");
      const id = next++;
      const rest = [];
      for (let i = 2; i < arguments.length; i++) rest.push(arguments[i]);
      pending.set(id, true);
      clock.sleep(delay === undefined ? 0 : delay).then(function () {
        if (!pending.has(id)) return;
        pending.delete(id);
        callback.apply(undefined, rest);
      });
      return id;
    };
    G.clearTimeout = function clearTimeout(id) { pending.delete(id); };
    G.setInterval = function setInterval(callback, delay) {
      if (typeof callback !== "function") throw new TypeError("setInterval: not a function");
      const id = next++;
      pending.set(id, true);
      const again = function () {
        if (!pending.has(id)) return;
        clock.sleep(delay === undefined ? 0 : delay).then(function () {
          if (!pending.has(id)) return;
          callback();
          again();
        });
      };
      again();
      return id;
    };
    G.clearInterval = function clearInterval(id) { pending.delete(id); };
  }
  // ---- timers -------------------------------------------------------
  // A timer is a capability: the deployment's clock holds the wait, and the
  // callback runs as an ordinary job when it answers. Without that grant
  // there are no timers at all, rather than timers that never fire.
  if (typeof clock === "object" && clock && typeof clock.sleep === "function") {
    const pending = new Map();
    let next = 1;
    G.setTimeout = function setTimeout(callback, delay) {
      if (typeof callback !== "function") throw new TypeError("setTimeout: not a function");
      const id = next++;
      const rest = [];
      for (let i = 2; i < arguments.length; i++) rest.push(arguments[i]);
      pending.set(id, true);
      clock.sleep(delay === undefined ? 0 : delay).then(function () {
        if (!pending.has(id)) return;
        pending.delete(id);
        callback.apply(undefined, rest);
      });
      return id;
    };
    G.clearTimeout = function clearTimeout(id) { pending.delete(id); };
    G.setInterval = function setInterval(callback, delay) {
      if (typeof callback !== "function") throw new TypeError("setInterval: not a function");
      const id = next++;
      pending.set(id, true);
      const again = function () {
        if (!pending.has(id)) return;
        clock.sleep(delay === undefined ? 0 : delay).then(function () {
          if (!pending.has(id)) return;
          callback();
          again();
        });
      };
      again();
      return id;
    };
    G.clearInterval = function clearInterval(id) { pending.delete(id); };
  }

  // ---- bytes and text --------------------------------------------------
  // A payload crosses as bytes, and a string carrying one is one byte a
  // character. Text is a reading of those bytes rather than what they are,
  // so it is decoded here: the seam does not guess which it was given.
  function bytesToText(bytes) {
    let out = "";
    let at = 0;
    while (at < bytes.length) {
      const first = bytes.charCodeAt(at);
      let point;
      let width;
      if (first < 0x80) { point = first; width = 1; }
      else if ((first & 0xE0) === 0xC0) { point = first & 0x1F; width = 2; }
      else if ((first & 0xF0) === 0xE0) { point = first & 0x0F; width = 3; }
      else if ((first & 0xF8) === 0xF0) { point = first & 0x07; width = 4; }
      else { out += "\uFFFD"; at += 1; continue; }
      if (at + width > bytes.length) { out += "\uFFFD"; break; }
      for (let i = 1; i < width; i++) {
        point = (point << 6) | (bytes.charCodeAt(at + i) & 0x3F);
      }
      at += width;
      if (point > 0xFFFF) {
        point -= 0x10000;
        out += String.fromCharCode(0xD800 + (point >> 10), 0xDC00 + (point & 0x3FF));
      } else {
        out += String.fromCharCode(point);
      }
    }
    return out;
  }

  function textToBytes(text) {
    let out = "";
    for (let i = 0; i < text.length; i++) {
      let point = text.charCodeAt(i);
      if (point >= 0xD800 && point < 0xDC00 && i + 1 < text.length) {
        const low = text.charCodeAt(i + 1);
        if (low >= 0xDC00 && low < 0xE000) {
          point = 0x10000 + ((point - 0xD800) << 10) + (low - 0xDC00);
          i++;
        }
      }
      if (point < 0x80) out += String.fromCharCode(point);
      else if (point < 0x800) {
        out += String.fromCharCode(0xC0 | (point >> 6), 0x80 | (point & 0x3F));
      } else if (point < 0x10000) {
        out += String.fromCharCode(
          0xE0 | (point >> 12), 0x80 | ((point >> 6) & 0x3F), 0x80 | (point & 0x3F));
      } else {
        out += String.fromCharCode(
          0xF0 | (point >> 18), 0x80 | ((point >> 12) & 0x3F),
          0x80 | ((point >> 6) & 0x3F), 0x80 | (point & 0x3F));
      }
    }
    return out;
  }

  G.__bytesToText = bytesToText;
  G.__textToBytes = textToBytes;

  // ---- fetch --------------------------------------------------------
  // HTTP over the binding a deployment granted. What protocol carried the
  // request, which version it negotiated and how the body was framed on the
  // wire are the provider's business and reach nothing here: this composes a
  // request, hands it over, and reads the response back through the handle it
  // was answered with.
  //
  // The response is a resource, so its body has no length this has to know in
  // advance and no buffer here has to hold all of one.
  if (typeof http === "object" && http && typeof http.send === "function") {
    G.Headers = class Headers {
      constructor(init) {
        this._pairs = [];
        if (init instanceof G.Headers) {
          init.forEach((value, name) => this.append(name, value));
        } else if (init && typeof init === "object") {
          const keys = Object.keys(init);
          for (let i = 0; i < keys.length; i++) this.set(keys[i], init[keys[i]]);
        }
      }
      get(name) {
        const key = String(name).toLowerCase();
        for (let i = 0; i < this._pairs.length; i++) {
          if (this._pairs[i][0] === key) return this._pairs[i][1];
        }
        return null;
      }
      has(name) { return this.get(name) !== null; }
      set(name, value) {
        const key = String(name).toLowerCase();
        for (let i = 0; i < this._pairs.length; i++) {
          if (this._pairs[i][0] === key) { this._pairs[i][1] = String(value); return; }
        }
        this._pairs.push([key, String(value)]);
      }
      append(name, value) { this._pairs.push([String(name).toLowerCase(), String(value)]); }
      forEach(fn) {
        for (let i = 0; i < this._pairs.length; i++) fn(this._pairs[i][1], this._pairs[i][0], this);
      }
    };

    // The header block a response came with, as fields rather than bytes.
    function parseFields(block) {
      const headers = new G.Headers();
      const lines = block.split("\r\n");
      for (let i = 0; i < lines.length; i++) {
        const at = lines[i].indexOf(":");
        if (at > 0) headers.set(lines[i].substring(0, at), lines[i].substring(at + 1).trim());
      }
      return headers;
    }

    // Read the whole body through the handle. Each read answers what is
    // there; nothing answers that the body is done, which is the one thing a
    // reader cannot infer from a pause.
    function drain(handle, held) {
      return http.read(handle, 8192).then(function (chunk) {
        if (chunk.length === 0) return held;
        return drain(handle, held + chunk);
      });
    }

    G.Response = class Response {
      constructor(body, options) {
        const settings = options === undefined ? {} : options;
        this._body = body === undefined ? "" : String(body);
        this.status = settings.status === undefined ? 200 : settings.status;
        this.statusText = settings.statusText === undefined ? "" : String(settings.statusText);
        this.headers = settings.headers instanceof G.Headers
          ? settings.headers
          : new G.Headers(settings.headers);
        this.ok = this.status >= 200 && this.status < 300;
        this.url = settings.url === undefined ? "" : String(settings.url);
        this.bodyUsed = false;
      }
      // `_body` holds bytes. Reading it as text is a decode, and a body that
      // is not text keeps its bytes.
      text() { this.bodyUsed = true; return Promise.resolve(G.__bytesToText(this._body)); }
      json() {
        const body = this._body;
        this.bodyUsed = true;
        return Promise.resolve(JSON.parse(G.__bytesToText(body)));
      }
      bytes() { this.bodyUsed = true; return Promise.resolve(this._body); }
      arrayBuffer() {
        const body = this._body;
        this.bodyUsed = true;
        const out = new Uint8Array(body.length);
        for (let i = 0; i < body.length; i++) out[i] = body.charCodeAt(i);
        return Promise.resolve(out.buffer);
      }
    };

    G.fetch = function fetch(resource, options) {
      const settings = options === undefined ? {} : options;
      let path = String(resource);
      let named = null;
      // A deployment wires one origin. A URL naming another is refused rather
      // than sent to the one that was wired: answering it would hand a
      // program one origin's response while it believed it was reading
      // another's, which is worse than not answering at all.
      if (path.indexOf("://") >= 0) {
        const target = new G.URL(path);
        named = target.host;
        path = target.pathname + (target.search === undefined ? "" : target.search);
      }
      if (path.length === 0 || path[0] !== "/") path = "/" + path;
      const method = settings.method === undefined ? "GET" : String(settings.method).toUpperCase();
      const body = settings.body === undefined ? "" : G.__textToBytes(String(settings.body));
      let block = "";
      if (settings.headers) {
        new G.Headers(settings.headers).forEach(function (value, name) {
          block += name + ": " + value + "\r\n";
        });
      }
      let handle = null;
      return http.origin().then(function (origin) {
        if (named !== null && named !== origin) {
          throw new TypeError(
            "fetch: " + named + " is not the granted origin " + origin);
        }
        return http.send(method, path, block, body);
      }).then(function (opened) {
        handle = opened;
        return http.status(handle);
      }).then(function (status) {
        return http.headers(handle).then(function (block) {
          return drain(handle, "").then(function (body) {
            return http.close(handle).then(function () {
              return new G.Response(body, {
                status: status,
                headers: parseFields(G.__bytesToText(block)),
                url: String(resource),
              });
            });
          });
        });
      });
    };
  }

  // ---- WebSocket -------------------------------------------------------
  // RFC 6455 over the same connection `fetch` uses. It needs no capability
  // of its own: a WebSocket is an HTTP request that changes protocol, and
  // the protocol above it is frames this file writes and reads. Masking is
  // required of a client and must be unpredictable, so it needs randomness
  // the deployment granted -- without `entropy` there is no WebSocket, for
  // the same reason there is no `fetch` without `net`.
  if (typeof net === "object" && net && typeof entropy === "object" && entropy) {
    // SHA-1 over a byte string. Present only to check the server's
    // `Sec-WebSocket-Accept`, which is the one part of the handshake that
    // proves the peer read the key rather than echoing a constant.
    function sha1(bytes) {
      const h = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
      const total = bytes.length;
      let padded = bytes + String.fromCharCode(0x80);
      while (padded.length % 64 !== 56) padded += String.fromCharCode(0);
      const bits = total * 8;
      for (let i = 7; i >= 0; i--) {
        padded += String.fromCharCode(Math.floor(bits / Math.pow(2, i * 8)) & 0xFF);
      }
      const w = new Array(80);
      for (let at = 0; at < padded.length; at += 64) {
        for (let i = 0; i < 16; i++) {
          w[i] = (padded.charCodeAt(at + i * 4) << 24)
            | (padded.charCodeAt(at + i * 4 + 1) << 16)
            | (padded.charCodeAt(at + i * 4 + 2) << 8)
            | padded.charCodeAt(at + i * 4 + 3);
        }
        for (let i = 16; i < 80; i++) {
          const v = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16];
          w[i] = (v << 1) | (v >>> 31);
        }
        let [a, b, c, d, e] = h;
        for (let i = 0; i < 80; i++) {
          let f;
          let k;
          if (i < 20) { f = (b & c) | (~b & d); k = 0x5A827999; }
          else if (i < 40) { f = b ^ c ^ d; k = 0x6ED9EBA1; }
          else if (i < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8F1BBCDC; }
          else { f = b ^ c ^ d; k = 0xCA62C1D6; }
          const t = (((a << 5) | (a >>> 27)) + f + e + k + w[i]) | 0;
          e = d; d = c; c = (b << 30) | (b >>> 2); b = a; a = t;
        }
        h[0] = (h[0] + a) | 0; h[1] = (h[1] + b) | 0; h[2] = (h[2] + c) | 0;
        h[3] = (h[3] + d) | 0; h[4] = (h[4] + e) | 0;
      }
      let out = "";
      for (let i = 0; i < 5; i++) {
        out += String.fromCharCode((h[i] >>> 24) & 0xFF, (h[i] >>> 16) & 0xFF,
          (h[i] >>> 8) & 0xFF, h[i] & 0xFF);
      }
      return out;
    }

    const ACCEPT_MAGIC = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

    // Four bytes from the granted source. A mask a peer can predict is no
    // mask at all, so this is a call and not a counter.
    function maskKey() {
      return entropy.random().then(function (value) {
        let held = Math.abs(Math.floor(value));
        let out = "";
        for (let i = 0; i < 4; i++) {
          out += String.fromCharCode(held & 0xFF);
          held = Math.floor(held / 256);
        }
        return out;
      });
    }

    function frame(opcode, payload, mask) {
      let head = String.fromCharCode(0x80 | opcode);
      const length = payload.length;
      if (length < 126) head += String.fromCharCode(0x80 | length);
      else if (length < 65536) {
        head += String.fromCharCode(0x80 | 126, (length >> 8) & 0xFF, length & 0xFF);
      } else {
        head += String.fromCharCode(0x80 | 127, 0, 0, 0, 0,
          (length >>> 24) & 0xFF, (length >>> 16) & 0xFF,
          (length >>> 8) & 0xFF, length & 0xFF);
      }
      let masked = "";
      for (let i = 0; i < length; i++) {
        masked += String.fromCharCode(payload.charCodeAt(i) ^ mask.charCodeAt(i % 4));
      }
      return head + mask + masked;
    }

    // One frame off the front of `buffer`, or nothing when it is not all
    // here yet. A server never masks, which RFC 6455 requires and this
    // checks rather than assumes.
    function unframe(buffer) {
      if (buffer.length < 2) return null;
      const first = buffer.charCodeAt(0);
      const second = buffer.charCodeAt(1);
      if ((second & 0x80) !== 0) return { fault: "masked frame from server" };
      let length = second & 0x7F;
      let at = 2;
      if (length === 126) {
        if (buffer.length < 4) return null;
        length = (buffer.charCodeAt(2) << 8) | buffer.charCodeAt(3);
        at = 4;
      } else if (length === 127) {
        if (buffer.length < 10) return null;
        length = 0;
        for (let i = 2; i < 10; i++) length = length * 256 + buffer.charCodeAt(i);
        at = 10;
      }
      if (buffer.length < at + length) return null;
      return {
        fin: (first & 0x80) !== 0,
        opcode: first & 0x0F,
        payload: buffer.substring(at, at + length),
        rest: buffer.substring(at + length),
      };
    }

    G.WebSocket = class WebSocket extends G.EventTarget {
      constructor(url) {
        super();
        this.url = String(url);
        this.readyState = 0;
        this.bufferedAmount = 0;
        this._handle = null;
        this._buffer = "";
        this._fragments = "";
        this._fragmentOpcode = 0;
        const socket = this;
        net.endpoint().then(function (endpoint) {
          let path = socket.url;
          const scheme = /^wss?:\/\//i.exec(path);
          if (scheme !== null) {
            const rest = path.substring(scheme[0].length);
            const cut = rest.indexOf("/");
            const host = cut < 0 ? rest : rest.substring(0, cut);
            if (host !== endpoint) {
              throw new TypeError(
                "WebSocket: " + host + " is not the granted endpoint " + endpoint);
            }
            path = cut < 0 ? "/" : rest.substring(cut);
          }
          if (path.length === 0 || path[0] !== "/") path = "/" + path;
          return maskKey().then(function (a) {
            return maskKey().then(function (b) {
              return maskKey().then(function (c) {
                return maskKey().then(function (d) {
                  const nonce = G.btoa(a + b + c + d);
                  const request = "GET " + path + " HTTP/1.1\r\n"
                    + "Host: " + endpoint + "\r\n"
                    + "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                    + "Sec-WebSocket-Key: " + nonce + "\r\n"
                    + "Sec-WebSocket-Version: 13\r\n\r\n";
                  return net.connect().then(function (handle) {
                    socket._handle = handle;
                    return net.send(handle, request).then(function () {
                      return socket._handshake(nonce);
                    });
                  });
                });
              });
            });
          });
        }).then(undefined, function (error) { socket._fail(error); });
      }

      // Read until the response head is whole, then hold the server to the
      // accept it must have computed from the key.
      _handshake(nonce) {
        const socket = this;
        return net.receive(socket._handle, 4096).then(function (chunk) {
          if (chunk.length === 0) throw new Error("WebSocket: closed during handshake");
          socket._buffer += chunk;
          const split = socket._buffer.indexOf("\r\n\r\n");
          if (split < 0) return socket._handshake(nonce);
          const head = socket._buffer.substring(0, split);
          socket._buffer = socket._buffer.substring(split + 4);
          if (!/^HTTP\/1\.1\s+101/.test(head)) {
            throw new Error("WebSocket: not upgraded: " + head.split("\r\n")[0]);
          }
          const offered = /sec-websocket-accept:\s*(\S+)/i.exec(head);
          const wanted = G.btoa(sha1(nonce + ACCEPT_MAGIC));
          if (offered === null || offered[1] !== wanted) {
            throw new Error("WebSocket: the accept does not answer the key");
          }
          socket.readyState = 1;
          socket.dispatchEvent(new G.Event("open"));
          socket._pump();
          return undefined;
        });
      }

      _pump() {
        const socket = this;
        if (socket.readyState > 2) return;
        net.receive(socket._handle, 8192).then(function (chunk) {
          if (chunk.length === 0) { socket._shut(1006, ""); return; }
          socket._buffer += chunk;
          while (true) {
            const taken = unframe(socket._buffer);
            if (taken === null) break;
            if (taken.fault !== undefined) { socket._fail(new Error(taken.fault)); return; }
            socket._buffer = taken.rest;
            socket._take(taken);
          }
          socket._pump();
        }, function (error) { socket._fail(error); });
      }

      _take(held) {
        const socket = this;
        if (held.opcode === 8) {
          let code = 1005;
          if (held.payload.length >= 2) {
            code = (held.payload.charCodeAt(0) << 8) | held.payload.charCodeAt(1);
          }
          socket._shut(code, G.__bytesToText(held.payload.substring(2)));
          return;
        }
        if (held.opcode === 9) { socket._write(10, held.payload); return; }
        if (held.opcode === 10) return;
        if (held.opcode === 0) socket._fragments += held.payload;
        else { socket._fragmentOpcode = held.opcode; socket._fragments = held.payload; }
        if (!held.fin) return;
        const whole = socket._fragments;
        socket._fragments = "";
        const event = new G.Event("message");
        event.data = socket._fragmentOpcode === 2 ? whole : G.__bytesToText(whole);
        socket.dispatchEvent(event);
      }

      _write(opcode, payload) {
        const socket = this;
        return maskKey().then(function (mask) {
          return net.send(socket._handle, frame(opcode, payload, mask));
        });
      }

      send(data) {
        if (this.readyState !== 1) throw new Error("WebSocket: not open");
        return this._write(1, G.__textToBytes(String(data)));
      }

      close(code, reason) {
        if (this.readyState > 1) return;
        this.readyState = 2;
        const shut = code === undefined ? 1000 : code;
        let payload = String.fromCharCode((shut >> 8) & 0xFF, shut & 0xFF);
        if (reason !== undefined) payload += G.__textToBytes(String(reason));
        const socket = this;
        this._write(8, payload).then(function () { socket._shut(shut, reason); },
          function () { socket._shut(shut, reason); });
      }

      _shut(code, reason) {
        if (this.readyState === 3) return;
        this.readyState = 3;
        const socket = this;
        const done = function () {
          const event = new G.Event("close");
          event.code = code;
          event.reason = reason === undefined ? "" : reason;
          socket.dispatchEvent(event);
        };
        if (this._handle === null) { done(); return; }
        net.close(this._handle).then(done, done);
      }

      _fail(error) {
        const event = new G.Event("error");
        event.error = error;
        this.dispatchEvent(event);
        this._shut(1006, String(error));
      }
    };
    G.WebSocket.CONNECTING = 0;
    G.WebSocket.OPEN = 1;
    G.WebSocket.CLOSING = 2;
    G.WebSocket.CLOSED = 3;
  }

  // ---- structured clone ---------------------------------------------
  G.structuredClone = function structuredClone(value) {
    return clone(value, 0);
  };
  function clone(value, depth) {
    if (depth > 32) throw new RangeError("structuredClone: too deep");
    if (value === null || typeof value !== "object") return value;
    if (Array.isArray(value)) {
      const out = [];
      for (let i = 0; i < value.length; i++) out.push(clone(value[i], depth + 1));
      return out;
    }
    if (value instanceof Map) {
      const out = new Map();
      value.forEach(function (v, k) { out.set(clone(k, depth + 1), clone(v, depth + 1)); });
      return out;
    }
    if (value instanceof Set) {
      const out = new Set();
      value.forEach(function (v) { out.add(clone(v, depth + 1)); });
      return out;
    }
    if (value instanceof Date) return new Date(value.getTime());
    if (typeof value === "function") throw new TypeError("structuredClone: function");
    const out = {};
    const keys = Object.keys(value);
    for (let i = 0; i < keys.length; i++) out[keys[i]] = clone(value[keys[i]], depth + 1);
    return out;
  }
})();
"##;

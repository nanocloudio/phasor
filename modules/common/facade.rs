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
  // RFC 6455, served rather than written here. The upgrade, the accept it
  // verifies, the masking and the frame codec belong to the provider the
  // deployment wired: one implementation for every consumer on the platform,
  // instead of a second one in JavaScript paying for the SHA-1 of every
  // handshake out of the program's own fuel.
  //
  // What is left here is the shape the language promises -- an EventTarget
  // that opens, carries messages, and closes -- over four calls.
  if (typeof websocket === "object" && websocket) {
    G.WebSocket = class WebSocket extends G.EventTarget {
      constructor(url) {
        super();
        this.url = String(url);
        this.readyState = 0;
        this.bufferedAmount = 0;
        this.binaryType = "blob";
        this._handle = null;
        const socket = this;
        // The origin is the deployment's and the resource is the program's,
        // which is the same split `fetch` makes. A URL naming another host is
        // refused, because the alternative is handing a program one origin's
        // stream while it believes it reached another.
        let path = this.url;
        let named = null;
        const scheme = /^wss?:\/\//i.exec(path);
        if (scheme !== null) {
          const rest = path.substring(scheme[0].length);
          const cut = rest.indexOf("/");
          named = cut < 0 ? rest : rest.substring(0, cut);
          path = cut < 0 ? "/" : rest.substring(cut);
        }
        if (path.length === 0 || path[0] !== "/") path = "/" + path;
        websocket.origin().then(function (origin) {
          if (named !== null && named !== origin) {
            throw new TypeError(
              "WebSocket: " + named + " is not the granted origin " + origin);
          }
          return websocket.open(path);
        }).then(function (handle) {
          socket._handle = handle;
          socket.readyState = 1;
          socket.dispatchEvent({ type: "open" });
          socket._pump();
        }, function (error) {
          socket._fail(error);
        });
      }

      // One read outstanding at a time: the provider holds the next message
      // until this one is taken, so nothing is lost by not asking for two.
      _pump() {
        const socket = this;
        if (socket.readyState > 1 || socket._handle === null) return;
        websocket.receive(socket._handle).then(function (chunk) {
          const opcode = chunk.charCodeAt(0);
          const body = chunk.substring(1);
          if (opcode === 8) { socket._ended(1000, ""); return; }
          socket.dispatchEvent({
            type: "message",
            data: opcode === 2 ? G.__textToBytes(body) : G.__bytesToText(body),
          });
          socket._pump();
        }, function (error) { socket._fail(error); });
      }

      send(data) {
        if (this.readyState !== 1) throw new Error("WebSocket: not open");
        const binary = typeof data !== "string";
        const body = binary ? G.__bytesToText(data) : G.__textToBytes(data);
        return void websocket.send(this._handle, binary ? 2 : 1, body)
          .then(undefined, () => {});
      }

      close() {
        if (this.readyState > 1) return;
        this.readyState = 2;
        const socket = this;
        if (this._handle === null) { this._ended(1000, ""); return; }
        websocket.close(this._handle).then(function () {
          socket._ended(1000, "");
        }, function () { socket._ended(1006, ""); });
      }

      _ended(code, reason) {
        if (this.readyState === 3) return;
        this.readyState = 3;
        this.dispatchEvent({ type: "close", code: code, reason: reason, wasClean: code === 1000 });
      }

      _fail(error) {
        if (this.readyState === 3) return;
        this.dispatchEvent({ type: "error", error: error });
        this._ended(1006, "");
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

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

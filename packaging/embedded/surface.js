// The surface a program on a small part finds before it runs: `console` over
// the `print` the host installed, and timers over a granted clock.
//
// It is the façade's console and timers and nothing else. The whole façade is
// some 30 KB of source, and on a part whose heap is 96 KiB with 69 KiB of it
// taken by the realm, the objects it builds do not fit beside a program. What
// is here behaves as the façade's own does, so a program written against one
// runs against the other.
//
// `crypto.getRandomValues` and `Math.random` are not here: the isolate
// installs them itself when the task holds an entropy grant, from a generator
// it keys with a seed drawn from that grant before this script runs.
(function () {
  "use strict";
  const G = globalThis;

  if (typeof print === "function") {
    const write = print;
    const line = function (args) {
      const parts = [];
      for (let i = 0; i < args.length; i++) parts.push(String(args[i]));
      return parts.join(" ");
    };
    G.console = {
      log: function () { write(line(arguments)); },
      info: function () { write(line(arguments)); },
      warn: function () { write(line(arguments)); },
      error: function () { write(line(arguments)); },
      debug: function () { write(line(arguments)); },
    };
  }

  // A timer is a capability: the deployment's clock holds the wait, and the
  // callback runs as an ordinary job when it answers. Without that grant
  // there are no timers at all, rather than timers that never fire.
  if (typeof clock === "object" && clock && typeof clock.sleep === "function") {
    const pending = new Map();
    let next = 1;
    G.setTimeout = function setTimeout(callback, delay) {
      if (typeof callback !== "function") throw new TypeError("setTimeout: not a function");
      const id = next++;
      pending.set(id, true);
      clock.sleep(delay === undefined ? 0 : delay).then(function () {
        if (!pending.has(id)) return;
        pending.delete(id);
        callback();
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
})();

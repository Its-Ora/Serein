// Runs only in Serein's newly-created ephemeral authentication webview.
// Observe its own same-origin API Authorization header after the owner logs in.
// Discord still performs the entire authentication/challenge/QR flow.
// Fork: the same request's identity headers (X-Super-Properties, locale, timezone) are
// handed over too, so Serein keeps talking to Discord as the client that logged in.
(() => {
  if (window !== window.top || location.origin !== "https://discord.com") return;
  const capability = "__SEREIN_LOGIN_CAPABILITY__";
  // Only hosts that parse the identity fields enable this (the Linux bridge keeps the
  // original token-only message).
  const withIdentity = "__SEREIN_LOGIN_IDENTITY__" === "1";
  let delivered = false;
  const opened = Date.now();
  const allowed = value => {
    try {
      const url = new URL(value, location.href);
      return url.origin === "https://discord.com" && /^\/api\/v\d+\//.test(url.pathname);
    } catch { return false; }
  };
  const clean = (value, max) =>
    typeof value === "string" && value.length <= max && /^[\x21-\x7e]*$/.test(value) ? value : "";
  // headers: lower-case name -> value, from one authorized API request.
  const deliver = headers => {
    const token = headers.authorization;
    if (delivered || Date.now() - opened > 600000 || typeof token !== "string" || token.length < 16 || token.length > 2048 || /\s/.test(token)) return;
    delivered = true;
    if (!withIdentity) {
      window.ipc.postMessage(capability + token);
      return;
    }
    const identity = [
      clean(headers["x-super-properties"], 4096),
      clean(headers["x-discord-locale"], 16),
      clean(headers["x-discord-timezone"], 64),
    ];
    window.ipc.postMessage(capability + [token, ...identity].join("\n"));
  };
  const pending = new WeakMap();
  const open = XMLHttpRequest.prototype.open;
  const setHeader = XMLHttpRequest.prototype.setRequestHeader;
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function(method, url, ...rest) {
    if (allowed(url)) pending.set(this, {}); else pending.delete(this);
    return open.call(this, method, url, ...rest);
  };
  XMLHttpRequest.prototype.setRequestHeader = function(name, value) {
    const headers = pending.get(this);
    if (headers) {
      const key = String(name).toLowerCase();
      headers[key] = String(value);
      if (!withIdentity && key === "authorization") deliver({ authorization: String(value) });
    }
    return setHeader.call(this, name, value);
  };
  // All headers are set before send(), so the identity travels with the token.
  XMLHttpRequest.prototype.send = function(...args) {
    const headers = pending.get(this);
    if (headers && withIdentity) deliver(headers);
    return send.apply(this, args);
  };
  const originalFetch = window.fetch;
  window.fetch = function(input, init) {
    if (allowed(input instanceof Request ? input.url : input)) {
      const source = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined));
      const headers = {};
      source.forEach((value, name) => { headers[name.toLowerCase()] = value; });
      deliver(headers);
    }
    return originalFetch.apply(this, arguments);
  };
})();

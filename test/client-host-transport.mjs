import assert from "node:assert/strict";

export function hostTransports(baseUrl, transport = fetch) {
  const request = (url, init) => {
    assert.equal(new URL(url).origin, baseUrl, "SDK must remain bound to the owned server");
    return transport(url, {
      ...init,
      signal: AbortSignal.any([
        ...(init?.signal ? [init.signal] : []),
        AbortSignal.timeout(15_000),
      ]),
    });
  };
  const subscription = (url, init) => {
    assert.equal(new URL(url).origin, baseUrl, "SDK must remain bound to the owned server");
    return transport(url, init);
  };
  return { request, subscription };
}

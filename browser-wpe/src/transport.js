// Adapts the shared bridge's one-function `__wateruiSend` transport onto the
// extension's `__wateruiNativeSend`.
//
// WPE injects this into every document, a refused one included, so the page
// can call it. Both ends are therefore taken once, at document start, after the
// shared bridge script has defined its resolver and before any page script
// runs: a document that later replaces `__wateruiResolve` or
// `__wateruiNativeSend` does not receive the host's reply or refusal through
// them. A request the bridge never made settles nothing, since the bridge's
// resolver ignores ids it does not hold.
(function () {
  var resolve = globalThis.__wateruiResolve;
  var nativeSend = globalThis.__wateruiNativeSend;
  if (typeof resolve !== "function" || typeof nativeSend !== "function") {
    throw new Error(
      "the WaterUI WPE transport needs the shared bridge and the web-process extension in place",
    );
  }
  globalThis.__wateruiSend = function (envelope) {
    var request = JSON.parse(envelope);
    nativeSend(envelope).then(
      function (replyText) {
        var reply = JSON.parse(replyText);
        resolve(request.id, reply.ok, reply.payload);
      },
      function (error) {
        resolve(request.id, false, { message: String(error) });
      },
    );
  };
})();

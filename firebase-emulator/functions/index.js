// Callable functions used only by the Functions emulator in tests/live_endpoints.rs.
const functions = require("firebase-functions/v1");

// Echoes the payload back and reports whether the caller was authenticated, so the test can
// check that the SDK attaches the user's ID token to callable requests.
exports.helloWorld = functions.https.onCall((data, context) => {
  return {
    message: `Hello, ${data && data.message ? data.message : "anonymous"}!`,
    uid: context.auth ? context.auth.uid : null,
    echo: data === undefined ? null : data,
  };
});

// Reports the credential headers the callable request arrived with, so tests can prove the SDK
// actually attaches them. The App Check token is echoed verbatim (it is a fake token minted by the
// test); the ID token is only reported as present, never echoed.
exports.echoHeaders = functions.https.onCall((data, context) => {
  const headers = (context.rawRequest && context.rawRequest.headers) || {};
  const authorization = headers["authorization"] || "";
  return {
    appCheck: headers["x-firebase-appcheck"] || null,
    hasAuthorization: authorization.startsWith("Bearer ") && authorization.length > "Bearer ".length,
    uid: context.auth ? context.auth.uid : null,
    instanceIdToken: headers["firebase-instance-id-token"] ? true : false,
  };
});

// Always fails with a typed error so the SDK's error mapping can be exercised.
exports.alwaysFails = functions.https.onCall(() => {
  throw new functions.https.HttpsError("failed-precondition", "This callable always fails", {
    reason: "test-fixture",
  });
});

// ---- 2nd-gen callables used by the streaming / options tests ----
const { onCall, HttpsError } = require("firebase-functions/v2/https");

// Streams `count` chunks {n: i} then returns the sum. Non-streaming clients only get the result.
exports.streamNumbers = onCall({}, async (request, response) => {
  const count = (request.data && request.data.count) || 3;
  let total = 0;
  for (let i = 1; i <= count; i++) {
    total += i;
    if (request.acceptsStreaming) {
      response.sendChunk({ n: i });
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
  }
  return { total, streamed: Boolean(request.acceptsStreaming) };
});

// Sends one chunk and then fails, so the client sees a typed error mid-stream.
exports.streamThenFail = onCall({}, async (request, response) => {
  if (request.acceptsStreaming) {
    response.sendChunk({ n: 1 });
  }
  throw new HttpsError("resource-exhausted", "stream failed midway", { after: 1 });
});

// Waits `delayMs` before answering, for client timeout tests.
exports.slowEcho = onCall({}, async (request) => {
  const delayMs = (request.data && request.data.delayMs) || 2000;
  await new Promise((resolve) => setTimeout(resolve, delayMs));
  return { done: true, delayMs };
});

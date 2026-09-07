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

// Always fails with a typed error so the SDK's error mapping can be exercised.
exports.alwaysFails = functions.https.onCall(() => {
  throw new functions.https.HttpsError("failed-precondition", "This callable always fails", {
    reason: "test-fixture",
  });
});

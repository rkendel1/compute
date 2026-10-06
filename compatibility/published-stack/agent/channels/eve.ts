// The standard eve HTTP channel (POST /eve/v1/session and its stream, cancel and
// info routes), unchanged except for who may call it.
//
// `chip start` is a production server, where the framework default admits no
// one and Vercel OIDC does not apply. Callers present the bearer token the
// operator sets in COMPUTE_CONFIGURED_CHIP_TOKEN; with no token set, the server
// refuses every request. `localDev()` stays last so `chip invoke` and `chip dev`
// (which boot a local development host) keep working; it admits nothing under
// `chip start`.
//
// operationId deduplication is scoped to the authenticated principal, so every
// holder of the token shares one principal and therefore one operation space.
import { timingSafeEqual } from "node:crypto";

import { extractBearerToken, localDev, type AuthFn } from "@appport/chip/channels/auth";
import { eveChannel } from "@appport/chip/channels/eve";

const configuredToken: AuthFn<Request> = (request) => {
  const expected = process.env.COMPUTE_CONFIGURED_CHIP_TOKEN;
  if (!expected) return null;
  const presented = extractBearerToken(request.headers.get("authorization"));
  if (presented === null) return null;
  const a = Buffer.from(presented);
  const b = Buffer.from(expected);
  if (a.length !== b.length || !timingSafeEqual(a, b)) return null;
  return {
    attributes: {},
    authenticator: "compute-configured-token",
    principalId: "compute-configured",
    principalType: "service",
  };
};

export default eveChannel({
  auth: [configuredToken, localDev()],
});

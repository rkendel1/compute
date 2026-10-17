// Entry module of a Compute workload Worker: the shared harness, and the
// workload module bound beside it at deployment.
import workload from "./workload.wasm";
import { handle } from "./harness.mjs";

export default {
  fetch(request, env) {
    return handle(request, env, workload);
  },
};

// Compute's harness for Cloudflare Workers.
//
// Runs one WASI (preview 1) command module per request: arguments,
// environment and standard input arrive with the request; exit code,
// stdout and stderr return in the response. There is no filesystem, no
// socket, and no state kept between requests. The harness is the same for
// every workload; the module is bound beside it at deployment.
//
// Protocol: compute.cloudflare.invoke@1. Every request must carry
// `Authorization: Bearer <COMPUTE_INVOKE_TOKEN>`; the token is a secret
// binding, so a workers.dev URL alone does not let anyone run the workload.

export const HARNESS_VERSION = "compute-harness/1";
export const INVOKE_PROTOCOL = "compute.cloudflare.invoke@1";

const ERRNO = { SUCCESS: 0, BADF: 8, INVAL: 28, NOSYS: 52, SPIPE: 70 };

class Exit {
  constructor(code) {
    this.code = code;
  }
}

function json(status, body) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function sameToken(given, expected) {
  if (typeof given !== "string" || typeof expected !== "string" || expected.length === 0) {
    return false;
  }
  let difference = given.length ^ expected.length;
  for (let index = 0; index < expected.length; index += 1) {
    difference |= (given.charCodeAt(index) || 0) ^ expected.charCodeAt(index);
  }
  return difference === 0;
}

export function toBase64(bytes) {
  let text = "";
  for (let index = 0; index < bytes.length; index += 0x8000) {
    text += String.fromCharCode.apply(null, bytes.subarray(index, index + 0x8000));
  }
  return btoa(text);
}

export function fromBase64(text) {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

class Sink {
  constructor(limit) {
    this.limit = limit;
    this.chunks = [];
    this.kept = 0;
    this.total = 0;
  }

  write(bytes) {
    this.total += bytes.length;
    const room = this.limit - this.kept;
    if (room > 0) {
      const part = bytes.subarray(0, Math.min(room, bytes.length));
      this.chunks.push(part.slice());
      this.kept += part.length;
    }
  }

  bytes() {
    const out = new Uint8Array(this.kept);
    let offset = 0;
    for (const chunk of this.chunks) {
      out.set(chunk, offset);
      offset += chunk.length;
    }
    return out;
  }
}

// Run `module` as a WASI command. Returns what happened; never throws for
// anything the workload does.
export async function runWasi(module, { args, env, stdin, maxOutputBytes }) {
  const encoder = new TextEncoder();
  const argv = args.map((value) => encoder.encode(`${value}\0`));
  const environ = env.map(([name, value]) => encoder.encode(`${name}=${value}\0`));
  const stdout = new Sink(maxOutputBytes);
  const stderr = new Sink(maxOutputBytes);
  let stdinOffset = 0;
  let memory;

  const view = () => new DataView(memory.buffer);
  const bytes = () => new Uint8Array(memory.buffer);
  const putStrings = (strings, pointers, buffer) => {
    let at = buffer;
    strings.forEach((value, index) => {
      view().setUint32(pointers + index * 4, at, true);
      bytes().set(value, at);
      at += value.length;
    });
    return ERRNO.SUCCESS;
  };
  const sizes = (strings, countPointer, sizePointer) => {
    view().setUint32(countPointer, strings.length, true);
    view().setUint32(sizePointer, strings.reduce((sum, value) => sum + value.length, 0), true);
    return ERRNO.SUCCESS;
  };
  const iovecs = (pointer, length) => {
    const out = [];
    for (let index = 0; index < length; index += 1) {
      const base = pointer + index * 8;
      out.push([view().getUint32(base, true), view().getUint32(base + 4, true)]);
    }
    return out;
  };

  const implemented = {
    args_get: (pointers, buffer) => putStrings(argv, pointers, buffer),
    args_sizes_get: (count, size) => sizes(argv, count, size),
    environ_get: (pointers, buffer) => putStrings(environ, pointers, buffer),
    environ_sizes_get: (count, size) => sizes(environ, count, size),
    fd_write: (fd, iovs, length, written) => {
      const sink = fd === 1 ? stdout : fd === 2 ? stderr : null;
      if (!sink) return ERRNO.BADF;
      let total = 0;
      for (const [base, size] of iovecs(iovs, length)) {
        sink.write(bytes().subarray(base, base + size));
        total += size;
      }
      view().setUint32(written, total, true);
      return ERRNO.SUCCESS;
    },
    fd_read: (fd, iovs, length, read) => {
      if (fd !== 0) return ERRNO.BADF;
      let total = 0;
      for (const [base, size] of iovecs(iovs, length)) {
        const part = stdin.subarray(stdinOffset, stdinOffset + size);
        bytes().set(part, base);
        stdinOffset += part.length;
        total += part.length;
        if (part.length < size) break;
      }
      view().setUint32(read, total, true);
      return ERRNO.SUCCESS;
    },
    fd_close: (fd) => (fd <= 2 ? ERRNO.SUCCESS : ERRNO.BADF),
    fd_fdstat_get: (fd, pointer) => {
      if (fd > 2) return ERRNO.BADF;
      view().setUint8(pointer, 2); // character device
      view().setUint16(pointer + 2, 0, true);
      view().setBigUint64(pointer + 8, 0xffffffffffffffffn, true);
      view().setBigUint64(pointer + 16, 0xffffffffffffffffn, true);
      return ERRNO.SUCCESS;
    },
    fd_fdstat_set_flags: (fd) => (fd <= 2 ? ERRNO.SUCCESS : ERRNO.BADF),
    fd_seek: (fd) => (fd <= 2 ? ERRNO.SPIPE : ERRNO.BADF),
    fd_tell: (fd) => (fd <= 2 ? ERRNO.SPIPE : ERRNO.BADF),
    fd_sync: (fd) => (fd <= 2 ? ERRNO.SUCCESS : ERRNO.BADF),
    fd_prestat_get: () => ERRNO.BADF, // no preopened directories: no filesystem
    fd_prestat_dir_name: () => ERRNO.BADF,
    proc_exit: (code) => {
      throw new Exit(code);
    },
    proc_raise: () => ERRNO.NOSYS,
    sched_yield: () => ERRNO.SUCCESS,
    clock_res_get: (_id, pointer) => {
      view().setBigUint64(pointer, 1000000n, true);
      return ERRNO.SUCCESS;
    },
    clock_time_get: (id, _precision, pointer) => {
      if (id > 3) return ERRNO.INVAL;
      const now = id === 0 ? Date.now() : performance.now();
      view().setBigUint64(pointer, BigInt(Math.floor(now * 1e6)), true);
      return ERRNO.SUCCESS;
    },
    random_get: (pointer, length) => {
      for (let offset = 0; offset < length; offset += 65536) {
        crypto.getRandomValues(bytes().subarray(pointer + offset, pointer + Math.min(length, offset + 65536)));
      }
      return ERRNO.SUCCESS;
    },
  };
  // Everything else a module may import (paths, sockets, polling) is
  // answered ENOSYS: the capability does not exist here.
  const wasi = new Proxy(implemented, {
    get: (target, name) => (name in target ? target[name] : () => ERRNO.NOSYS),
  });

  const started = Date.now();
  let instance;
  try {
    instance = await WebAssembly.instantiate(module, { wasi_snapshot_preview1: wasi });
  } catch (error) {
    return { status: "invalid_module", message: String(error && error.message ? error.message : error) };
  }
  memory = instance.exports.memory;
  if (!(memory instanceof WebAssembly.Memory) || typeof instance.exports._start !== "function") {
    return { status: "invalid_module", message: "the module is not a WASI command: it must export memory and _start" };
  }
  let status = "exited";
  let exitCode = 0;
  let trap = null;
  try {
    instance.exports._start();
  } catch (error) {
    if (error instanceof Exit) {
      exitCode = error.code;
    } else {
      status = "trapped";
      exitCode = null;
      trap = String(error && error.message ? error.message : error);
    }
  }
  return {
    status,
    exit_code: exitCode,
    trap,
    stdout: stdout.bytes(),
    stdout_bytes: stdout.total,
    stderr: stderr.bytes(),
    stderr_bytes: stderr.total,
    duration_ms: Date.now() - started,
  };
}

export async function handle(request, env, module) {
  if (request.method !== "POST") {
    return json(405, { error: "method_not_allowed", message: "invoke with POST" });
  }
  const authorization = request.headers.get("authorization") || "";
  const token = authorization.startsWith("Bearer ") ? authorization.slice(7) : "";
  if (!sameToken(token, env.COMPUTE_INVOKE_TOKEN)) {
    return json(401, { error: "unauthorized", message: "this Worker runs only for the Compute that deployed it" });
  }
  let body;
  try {
    body = await request.json();
  } catch {
    return json(400, { error: "bad_request", message: "the body must be JSON" });
  }
  if (body.protocol !== INVOKE_PROTOCOL) {
    return json(400, { error: "protocol_unsupported", message: `expected ${INVOKE_PROTOCOL}` });
  }
  const args = Array.isArray(body.args) ? body.args.map(String) : [];
  const vars = Array.isArray(body.env) ? body.env.map(([name, value]) => [String(name), String(value)]) : [];
  const maxOutputBytes = Number.isSafeInteger(body.max_output_bytes) && body.max_output_bytes > 0 ? body.max_output_bytes : 1048576;
  let stdin;
  try {
    stdin = fromBase64(body.stdin || "");
  } catch {
    return json(400, { error: "bad_request", message: "stdin must be base64" });
  }
  const outcome = await runWasi(module, { args, env: vars, stdin, maxOutputBytes });
  const response = {
    protocol: INVOKE_PROTOCOL,
    harness: HARNESS_VERSION,
    workload: env.COMPUTE_WORKLOAD_DIGEST || null,
    execution_id: typeof body.execution_id === "string" ? body.execution_id : null,
    status: outcome.status,
  };
  if (outcome.status === "invalid_module") {
    return json(200, { ...response, message: outcome.message });
  }
  return json(200, {
    ...response,
    exit_code: outcome.exit_code,
    trap: outcome.trap,
    stdout: toBase64(outcome.stdout),
    stdout_bytes: outcome.stdout_bytes,
    stderr: toBase64(outcome.stderr),
    stderr_bytes: outcome.stderr_bytes,
    duration_ms: outcome.duration_ms,
  });
}

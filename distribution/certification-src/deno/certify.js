const op = Deno.args[0];
if (op === "stdin") await Deno.stdin.readable.pipeTo(Deno.stdout.writable);
else if (op === "exit") Deno.exit(7);
else if (op === "sleep") await new Promise((resolve) => setTimeout(resolve, 5000));
else if (op === "filesystem") {
  try { await Deno.readTextFile("/etc/passwd"); console.log("visible"); }
  catch { console.log("blocked"); }
}
else if (op === "network") {
  const permission = await Deno.permissions.query({ name: "net", host: "127.0.0.1:9" });
  console.log(permission.state === "granted" ? "visible" : "blocked");
}
else if (op === "certify") {
  let hostEnvironment = "missing";
  try { hostEnvironment = Deno.env.get("COMPUTE_HOST_SECRET") ?? "missing"; }
  catch (error) { if (!(error instanceof Deno.errors.NotCapable)) throw error; }
  const result = {
    input: await Deno.readTextFile(`${Deno.env.get("COMPUTE_WORK_DIR")}/hello.txt`),
    success: true,
    argument: Deno.args[1],
    environment: Deno.env.get("CERTIFICATION_ENV") ?? "missing",
    host_environment: hostEnvironment,
  };
  await Deno.writeTextFile(`${Deno.env.get("COMPUTE_OUTPUT_DIR")}/result.json`, JSON.stringify(result));
  console.log(JSON.stringify({ runtime: "deno", runtime_version: Deno.version.deno }));
  console.error("certification-stderr");
}

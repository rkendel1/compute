const op = Deno.args[0];
if (op === "stdin") await Deno.stdin.readable.pipeTo(Deno.stdout.writable);
else if (op === "exit") Deno.exit(7);
else if (op === "sleep") await new Promise((resolve) => setTimeout(resolve, 5000));
else if (op === "certify") {
  const result = {
    input: await Deno.readTextFile(`${Deno.env.get("COMPUTE_WORK_DIR")}/hello.txt`),
    success: true,
    argument: Deno.args[1],
    environment: Deno.env.get("CERTIFICATION_ENV") ?? "missing",
    host_environment: Deno.env.get("COMPUTE_HOST_SECRET") ?? "missing",
  };
  await Deno.writeTextFile(`${Deno.env.get("COMPUTE_OUTPUT_DIR")}/result.json`, JSON.stringify(result));
  console.log(JSON.stringify({ runtime: "deno", runtime_version: Deno.version.deno }));
  console.error("certification-stderr");
}

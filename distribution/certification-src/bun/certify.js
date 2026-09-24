const op = Bun.argv[2];
if (op === "stdin") await Bun.write(Bun.stdout, await Bun.stdin.bytes());
else if (op === "exit") process.exit(7);
else if (op === "sleep") await Bun.sleep(5000);
else if (op === "certify") {
  const result = {
    input: await Bun.file(`${process.env.COMPUTE_WORK_DIR}/hello.txt`).text(),
    success: true,
    argument: Bun.argv[3],
    environment: process.env.CERTIFICATION_ENV ?? "missing",
    host_environment: process.env.COMPUTE_HOST_SECRET ?? "missing",
  };
  await Bun.write(`${process.env.COMPUTE_OUTPUT_DIR}/result.json`, JSON.stringify(result));
  console.log(JSON.stringify({ runtime: "bun", runtime_version: Bun.version }));
  console.error("certification-stderr");
}

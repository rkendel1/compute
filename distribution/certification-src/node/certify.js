const fs = require("fs");
const op = process.argv[2];
if (op === "stdin") process.stdin.pipe(process.stdout);
else if (op === "exit") process.exit(7);
else if (op === "sleep") setTimeout(() => {}, 5000);
else if (op === "certify") {
  const result = {
    input: fs.readFileSync(`${process.env.COMPUTE_WORK_DIR}/hello.txt`, "utf8"),
    success: true,
    argument: process.argv[3],
    environment: process.env.CERTIFICATION_ENV ?? "missing",
    host_environment: process.env.COMPUTE_HOST_SECRET ?? "missing",
  };
  fs.writeFileSync(`${process.env.COMPUTE_OUTPUT_DIR}/result.json`, JSON.stringify(result));
  console.log(JSON.stringify({ runtime: "node", runtime_version: process.versions.node }));
  console.error("certification-stderr");
}

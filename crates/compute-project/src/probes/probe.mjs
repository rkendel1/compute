// Reports, for each package named on the command line, the version installed
// in the dependency environment this process runs in, or null when it is
// absent. With `--app <directory>`, it also asks the environment's own
// @appport/appboundry package what it makes of the application bundle in that
// directory: AppBoundry's certification and runtime-readiness APIs answer;
// this program only relays their verdict. Packages are read from their
// package.json files, so nothing depends on what a package chooses to export.
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const args = process.argv.slice(2);
let appDirectory = null;
const packages = [];
for (let i = 0; i < args.length; i += 1) {
  if (args[i] === "--app") appDirectory = args[++i];
  else packages.push(args[i]);
}

const roots = (process.env.NODE_PATH || "")
  .split(path.delimiter)
  .filter(Boolean)
  .flatMap((root) => [path.join(root, "node_modules"), root]);

function find(name) {
  for (const root of roots) {
    const directory = path.join(root, name);
    try {
      const manifest = JSON.parse(fs.readFileSync(path.join(directory, "package.json"), "utf8"));
      if (manifest.name === name && typeof manifest.version === "string") {
        return { directory, version: manifest.version };
      }
    } catch {
      // not in this root
    }
  }
  return null;
}

for (const name of packages) {
  const found = find(name);
  console.log(JSON.stringify({ package: name, version: found ? found.version : null }));
}

if (appDirectory !== null) {
  const platform = find("@appport/appboundry");
  if (!platform) {
    console.log(JSON.stringify({ application: { error: "package_missing" } }));
  } else {
    try {
      const api = await import(pathToFileURL(path.join(platform.directory, "dist", "index.js")).href);
      const readiness = await api.evaluateAppBoundryRuntimeReadiness(path.resolve(appDirectory), []);
      const artifact = readiness.artifact;
      console.log(
        JSON.stringify({
          application: {
            packageVersion: platform.version,
            certification: artifact.status,
            failedChecks: artifact.checks.filter((check) => check.status !== "PASS").map((check) => check.name),
            applicationId: artifact.applicationId ?? null,
            artifactHash: artifact.artifactHash ?? null,
            packageIdentity: artifact.packageIdentity ?? null,
            readiness: readiness.status,
            missingProviders: readiness.missingProviders.map((p) => `${p.capability}@${p.version}`),
          },
        }),
      );
    } catch (error) {
      const reason = String((error && (error.code || error.message)) || error).slice(0, 200);
      console.log(JSON.stringify({ application: { error: reason } }));
    }
  }
}

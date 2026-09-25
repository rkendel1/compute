//! Shared behavioral conformance tests for Compute runtime adapters.
//!
//! Runtime crates supply a small workload fixture; all assertions live here
//! and exercise only [`compute_core::RuntimeAdapter`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use compute_core::{
    EnvironmentVariable, ExecutionInputSource, ExecutionResult, ExecutionStatus, Input,
    NetworkPolicy, ResourceLimits, RuntimeAdapter, RuntimeKind, RuntimeSpec, Workload,
    WorkloadOutput,
};

#[derive(Debug, Clone, Copy)]
pub enum FixtureLanguage {
    Python,
    JavaScript,
    Ruby,
    Php,
    Shell,
    Java,
    Dotnet,
    Native,
}

impl FixtureLanguage {
    fn extension(self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::JavaScript => "js",
            Self::Ruby => "rb",
            Self::Php => "php",
            Self::Shell => "sh",
            Self::Java => "jar",
            Self::Dotnet => "dll",
            Self::Native => "native",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::Python => PYTHON_FIXTURE,
            Self::JavaScript => JAVASCRIPT_FIXTURE,
            Self::Ruby => RUBY_FIXTURE,
            Self::Php => PHP_FIXTURE,
            Self::Shell => SHELL_FIXTURE,
            Self::Java | Self::Dotnet | Self::Native => "",
        }
    }
}

/// Creates an executable implementing the conformance fixture protocol.
/// Adapters may provide scripts, checked-in binaries, or generated modules;
/// the behavioral assertions stay shared.
pub trait ConformanceFixture {
    fn prepare(&self, directory: &Path) -> PathBuf;
}

impl ConformanceFixture for FixtureLanguage {
    fn prepare(&self, directory: &Path) -> PathBuf {
        match self {
            Self::Java => return prepare_java(directory),
            Self::Dotnet => return prepare_dotnet(directory),
            Self::Native => return prepare_native(directory),
            _ => {}
        }
        let entrypoint = directory.join(format!("conformance.{}", self.extension()));
        fs::write(&entrypoint, self.source()).expect("write conformance fixture");
        entrypoint
    }
}

fn run_fixture_command(command: &mut std::process::Command, name: &str) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run {name}: {error}"));
    assert!(
        output.status.success(),
        "{name} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn prepare_java(directory: &Path) -> PathBuf {
    let source = directory.join("Conformance.java");
    fs::write(&source, JAVA_FIXTURE).expect("write Java fixture");
    run_fixture_command(
        std::process::Command::new("javac")
            .arg(&source)
            .current_dir(directory),
        "javac",
    );
    let jar = directory.join("conformance.jar");
    run_fixture_command(
        std::process::Command::new("jar")
            .args(["--create", "--file"])
            .arg(&jar)
            .args(["--main-class", "Conformance", "Conformance.class"])
            .current_dir(directory),
        "jar",
    );
    jar
}

fn prepare_dotnet(directory: &Path) -> PathBuf {
    fs::write(directory.join("Program.cs"), DOTNET_FIXTURE).expect("write .NET fixture");
    fs::write(
        directory.join("conformance.csproj"),
        r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net10.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings><Nullable>enable</Nullable></PropertyGroup></Project>"#,
    )
    .expect("write .NET project");
    let output = directory.join("dotnet-output");
    let cli_home = directory.join("dotnet-home");
    let packages = directory.join("nuget-packages");
    run_fixture_command(
        std::process::Command::new("dotnet")
            .env("DOTNET_CLI_HOME", &cli_home)
            .env("NUGET_PACKAGES", &packages)
            .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
            .args(["build", "--nologo", "--verbosity", "quiet", "--output"])
            .arg(&output)
            .arg(directory.join("conformance.csproj")),
        "dotnet build",
    );
    output.join("conformance.dll")
}

fn prepare_native(directory: &Path) -> PathBuf {
    let source = directory.join("conformance.c");
    let executable = directory.join("conformance-native");
    fs::write(&source, NATIVE_FIXTURE).expect("write native fixture");
    run_fixture_command(
        std::process::Command::new("cc")
            .args(["-std=c11", "-O2"])
            .arg(&source)
            .arg("-o")
            .arg(&executable),
        "C compiler",
    );
    executable
}

/// Run the common contract against an available runtime adapter.
///
/// Panics include the runtime, case, expected and actual result, execution
/// configuration, and declared capabilities so CI failures are actionable.
pub async fn run_contract(adapter: &dyn RuntimeAdapter, fixture: &dyn ConformanceFixture) {
    let availability = adapter.availability(None).await;
    if !availability.available {
        assert_ne!(
            std::env::var("COMPUTE_REQUIRE_ALL_RUNTIMES").as_deref(),
            Ok("1"),
            "runtime {} is required for distribution conformance but is unavailable: {:?}",
            adapter.kind(),
            availability.remediation
        );
        return;
    }

    let temp = tempfile::tempdir().expect("create conformance workspace");
    let entrypoint = fixture.prepare(temp.path());
    let capabilities = adapter.capabilities();
    let network = capabilities
        .network
        .iter()
        .find_map(|(policy, capability)| capability.supported.then_some(policy.clone()))
        .unwrap_or_else(|| {
            fail(
                adapter,
                "network declaration",
                &"one supported policy",
                "none declared",
            )
        });

    for args in [
        vec![],
        vec!["one".into()],
        vec!["hello world".into(), "αβγ".into(), "--example".into()],
        vec!["".into()],
    ] {
        let mut operation = vec!["args".to_string()];
        operation.extend(args.clone());
        let result = execute(
            adapter,
            workload(adapter.kind(), &entrypoint, operation, network.clone()),
        )
        .await;
        let observed: Vec<String> = serde_json::from_str(result.stdout.text.trim())
            .unwrap_or_else(|error| fail(adapter, "arguments", &args, error));
        check(adapter, "arguments", &args, &observed, observed == args);
    }

    if capabilities.stdin.supported {
        for input in [
            Vec::new(),
            b"short text".to_vec(),
            b"first line\nsecond line\n".to_vec(),
            "unicode αβγ 🌍".as_bytes().to_vec(),
            vec![b'x'; 128 * 1024],
        ] {
            let mut request = workload(
                adapter.kind(),
                &entrypoint,
                vec!["stdin".into()],
                network.clone(),
            );
            request.stdin = input.clone();
            let result = execute(adapter, request).await;
            check(
                adapter,
                "stdin",
                &format!("{} bytes", input.len()),
                &result.stdout.bytes,
                result.stdout.text.as_bytes() == input,
            );
        }
    }

    let result = execute(
        adapter,
        workload(
            adapter.kind(),
            &entrypoint,
            vec!["streams".into()],
            network.clone(),
        ),
    )
    .await;
    check(
        adapter,
        "stdout",
        &"out α\nsecond line with spaces\n",
        &result.stdout.text,
        result.stdout.text == "out α\nsecond line with spaces\n",
    );
    check(
        adapter,
        "stderr",
        &"err β\nsecond error line\n",
        &result.stderr.text,
        result.stderr.text == "err β\nsecond error line\n",
    );

    for code in [0, 1, 2] {
        let result = execute(
            adapter,
            workload(
                adapter.kind(),
                &entrypoint,
                vec!["exit".into(), code.to_string()],
                network.clone(),
            ),
        )
        .await;
        check(
            adapter,
            "exit status",
            &ExecutionStatus::Completed,
            &result.status,
            result.status == ExecutionStatus::Completed,
        );
        check(
            adapter,
            "exit code",
            &Some(code),
            &result.exit_code,
            result.exit_code == Some(code),
        );
        check(
            adapter,
            "output on non-zero exit",
            &"output before exit\n",
            &result.stdout.text,
            result.stdout.text == "output before exit\n",
        );
    }

    if capabilities.environment.supported {
        let mut environment = workload(
            adapter.kind(),
            &entrypoint,
            vec!["env".into()],
            network.clone(),
        );
        environment.env = vec![
            EnvironmentVariable {
                key: "COMPUTE_TEST_VALUE".into(),
                value: "expected α".into(),
            },
            EnvironmentVariable {
                key: "COMPUTE_EMPTY_VALUE".into(),
                value: String::new(),
            },
        ];
        let result = execute(adapter, environment).await;
        check(
            adapter,
            "environment",
            &"expected α||missing",
            &result.stdout.text.trim(),
            result.stdout.text.trim() == "expected α||missing",
        );
    }

    if capabilities.artifacts.supported {
        let result = execute(
            adapter,
            workload(
                adapter.kind(),
                &entrypoint,
                vec!["artifact".into()],
                network.clone(),
            ),
        )
        .await;
        check(
            adapter,
            "artifact count",
            &2,
            &result.artifacts.len(),
            result.artifacts.len() == 2,
        );

        let mut request = workload(
            adapter.kind(),
            &entrypoint,
            vec!["portable-io".into()],
            network.clone(),
        );
        request.inputs.push(Input {
            path: "data/input.txt".into(),
            source: ExecutionInputSource::Inline {
                data: b"portable hello".to_vec(),
            },
        });
        request.outputs.push(WorkloadOutput {
            path: "data/output.txt".into(),
            required: true,
        });
        let result = execute(adapter, request).await;
        check(
            adapter,
            "portable input/output",
            &"portable hello",
            &result.outputs,
            result.status == ExecutionStatus::Completed
                && result.outputs.len() == 1
                && result.outputs[0].path == Path::new("data/output.txt")
                && result.outputs[0].data == b"portable hello",
        );
        check(
            adapter,
            "artifact paths",
            &vec!["/output/report.txt", "/output/result.json"],
            &result.artifacts,
            result
                .artifacts
                .iter()
                .all(|artifact| artifact.path.starts_with("/output"))
                && result.artifacts.iter().all(|artifact| artifact.size > 0),
        );
    }

    if capabilities.filesystem_isolation.supported {
        let declared = temp.path().join("declared.txt");
        fs::write(&declared, "declared α").expect("write declared input");
        fs::write(temp.path().join("host-secret"), "secret").expect("write host probe");
        let mut request = workload(
            adapter.kind(),
            &entrypoint,
            vec!["filesystem".into()],
            network.clone(),
        );
        request.inputs.push(Input {
            path: "declared.txt".into(),
            source: ExecutionInputSource::File { path: declared },
        });
        let result = execute(adapter, request).await;
        check(
            adapter,
            "filesystem isolation",
            &"declared α|blocked|blocked",
            &result.stdout.text,
            result.stdout.text == "declared α|blocked|blocked",
        );
    }

    let first = execute(
        adapter,
        workload(
            adapter.kind(),
            &entrypoint,
            vec!["empty".into()],
            network.clone(),
        ),
    )
    .await;
    let second = execute(
        adapter,
        workload(
            adapter.kind(),
            &entrypoint,
            vec!["empty".into()],
            network.clone(),
        ),
    )
    .await;
    check(
        adapter,
        "execution id",
        &"unique non-empty IDs",
        &(&first.execution_id, &second.execution_id),
        !first.execution_id.is_empty() && first.execution_id != second.execution_id,
    );
    assert_lifecycle(adapter, &first);
    let (third, fourth) = tokio::join!(
        execute(
            adapter,
            workload(
                adapter.kind(),
                &entrypoint,
                vec!["empty".into()],
                network.clone(),
            )
        ),
        execute(
            adapter,
            workload(
                adapter.kind(),
                &entrypoint,
                vec!["empty".into()],
                network.clone(),
            )
        )
    );
    check(
        adapter,
        "concurrent execution ids",
        &"unique IDs",
        &(&third.execution_id, &fourth.execution_id),
        third.execution_id != fourth.execution_id,
    );
    check(
        adapter,
        "empty output",
        &"empty stdout, stderr, and artifacts",
        &first,
        first.stdout.text.is_empty() && first.stderr.text.is_empty() && first.artifacts.is_empty(),
    );

    if capabilities.stdout_limit.supported {
        let mut limited = workload(
            adapter.kind(),
            &entrypoint,
            vec!["large-output".into()],
            network.clone(),
        );
        limited.resources.stdout_bytes = Some(1024);
        let result = execute(adapter, limited).await;
        check(
            adapter,
            "stdout limit",
            &true,
            &result.stdout.truncated,
            result.stdout.truncated
                && result.stdout.text.len() == 1024
                && result.stdout.bytes > 1024,
        );
    }

    if capabilities.stderr_limit.supported {
        let mut limited = workload(
            adapter.kind(),
            &entrypoint,
            vec!["large-stderr".into()],
            network.clone(),
        );
        limited.resources.stderr_bytes = Some(1024);
        let result = execute(adapter, limited).await;
        check(
            adapter,
            "stderr limit",
            &true,
            &result.stderr.truncated,
            result.stderr.truncated
                && result.stderr.text.len() == 1024
                && result.stderr.bytes > 1024,
        );
    }

    if capabilities.memory_limit.supported {
        let mut limited = workload(
            adapter.kind(),
            &entrypoint,
            vec!["memory".into()],
            network.clone(),
        );
        limited.resources.memory_bytes = Some(4 * 65_536);
        let result = execute(adapter, limited).await;
        check(
            adapter,
            "memory limit",
            &"limited",
            &result.stdout.text,
            result.status == ExecutionStatus::Completed && result.stdout.text == "limited",
        );
    }

    if capabilities.timeout.supported {
        let mut timeout = workload(
            adapter.kind(),
            &entrypoint,
            vec!["sleep".into()],
            network.clone(),
        );
        timeout.resources.wall_time = Some(Duration::from_millis(100));
        let result = execute(adapter, timeout).await;
        check(
            adapter,
            "timeout",
            &ExecutionStatus::TimedOut,
            &result,
            result.status == ExecutionStatus::TimedOut && result.error.is_some(),
        );
        assert_lifecycle(adapter, &result);
    }

    let missing = temp.path().join("missing-entrypoint");
    let result = execute(adapter, workload(adapter.kind(), &missing, vec![], network)).await;
    check(
        adapter,
        "startup failure",
        &"structured pre-start failure",
        &result,
        result.status == ExecutionStatus::Failed
            && !result.execution_id.is_empty()
            && result.error.as_ref().is_some_and(|error| !error.started),
    );
}

fn workload(
    kind: RuntimeKind,
    entrypoint: &Path,
    args: Vec<String>,
    network: NetworkPolicy,
) -> Workload {
    Workload {
        runtime: RuntimeSpec {
            kind,
            version: None,
        },
        entrypoint: entrypoint.to_path_buf(),
        args,
        stdin: vec![],
        env: vec![],
        inputs: vec![],
        outputs: vec![],
        mounts: vec![],
        network,
        resources: ResourceLimits::default(),
        isolation: compute_core::IsolationProfile::Process,
        host_isolation: compute_core::HostProfile::Trusted,
        dependencies: None,
    }
}

async fn execute(adapter: &dyn RuntimeAdapter, request: Workload) -> ExecutionResult {
    adapter
        .capabilities()
        .validate(adapter.kind(), &request)
        .unwrap_or_else(|error| fail(adapter, "capability validation", &request, error));
    let runtime = adapter
        .resolve(&request)
        .await
        .unwrap_or_else(|error| fail(adapter, "resolution", &request, error));
    adapter
        .execute(&request, &runtime)
        .await
        .unwrap_or_else(|error| fail(adapter, "execution", &request, error))
}

fn assert_lifecycle(adapter: &dyn RuntimeAdapter, result: &ExecutionResult) {
    let terminal = result
        .lifecycle
        .iter()
        .filter(|state| {
            matches!(
                state,
                ExecutionStatus::Completed
                    | ExecutionStatus::Failed
                    | ExecutionStatus::Cancelled
                    | ExecutionStatus::TimedOut
                    | ExecutionStatus::Killed
            )
        })
        .count();
    let unique = result.lifecycle.windows(2).all(|pair| pair[0] != pair[1]);
    check(
        adapter,
        "lifecycle",
        &"one terminal state and no adjacent duplicates",
        &result.lifecycle,
        terminal == 1 && unique && result.lifecycle.last() == Some(&result.status),
    );
}

fn check(
    adapter: &dyn RuntimeAdapter,
    test: &str,
    expected: &impl std::fmt::Debug,
    actual: &impl std::fmt::Debug,
    condition: bool,
) {
    if !condition {
        panic!(
            "Runtime conformance failure\nruntime: {}\ntest: {test}\nexpected: {expected:?}\nactual: {actual:?}\ncapabilities: {:#?}",
            adapter.kind(),
            adapter.capabilities()
        );
    }
}

fn fail(
    adapter: &dyn RuntimeAdapter,
    test: &str,
    configuration: &impl std::fmt::Debug,
    error: impl std::fmt::Display,
) -> ! {
    panic!(
        "Runtime conformance failure\nruntime: {}\ntest: {test}\nexecution configuration: {configuration:?}\nactual error: {error}\ncapabilities: {:#?}",
        adapter.kind(),
        adapter.capabilities()
    )
}

const PYTHON_FIXTURE: &str = r#"import json, os, sys, time
op = sys.argv[1]
if op == "args": print(json.dumps(sys.argv[2:], ensure_ascii=False))
elif op == "stdin": sys.stdout.buffer.write(sys.stdin.buffer.read())
elif op == "streams": print("out α\nsecond line with spaces"); print("err β\nsecond error line", file=sys.stderr)
elif op == "exit": print("output before exit"); sys.exit(int(sys.argv[2]))
elif op == "env": print(os.environ.get("COMPUTE_TEST_VALUE", "missing") + "|" + os.environ.get("COMPUTE_EMPTY_VALUE", "missing") + "|" + os.environ.get("COMPUTE_UNDECLARED_VALUE", "missing"))
elif op == "artifact":
    out = os.environ["COMPUTE_OUTPUT_DIR"]
    open(os.path.join(out, "result.json"), "w").write('{"ok":true}')
    open(os.path.join(out, "report.txt"), "w").write("report")
elif op == "portable-io":
    data = open(os.path.join(os.environ["COMPUTE_WORK_DIR"], "data/input.txt"), "rb").read()
    open(os.path.join(os.environ["COMPUTE_OUTPUT_DIR"], "data/output.txt"), "wb").write(data)
elif op == "large-output": sys.stdout.write("x" * 8192)
elif op == "large-stderr": sys.stderr.write("x" * 8192)
elif op == "sleep": print("started", flush=True); time.sleep(5)
"#;

const JAVASCRIPT_FIXTURE: &str = r#"const deno = globalThis.Deno;
const argv = deno ? deno.args : process.argv.slice(2);
const op = argv[0];
const env = (key) => deno ? Deno.env.get(key) : process.env[key];
const writeFile = async (path, value) => deno ? Deno.writeTextFile(path, value) : require('fs').writeFileSync(path, value);
const readFile = async (path) => deno ? Deno.readFile(path) : require('fs').readFileSync(path);
(async () => {
if (op === 'args') console.log(JSON.stringify(argv.slice(1)));
else if (op === 'stdin') { const data = deno ? await new Response(Deno.stdin.readable).arrayBuffer() : require('fs').readFileSync(0); if (deno) await Deno.stdout.write(new Uint8Array(data)); else process.stdout.write(data); }
else if (op === 'streams') { console.log('out α\nsecond line with spaces'); console.error('err β\nsecond error line'); }
else if (op === 'exit') { console.log('output before exit'); if (deno) Deno.exit(Number(argv[1])); else process.exit(Number(argv[1])); }
else if (op === 'env') console.log(`${env('COMPUTE_TEST_VALUE') ?? 'missing'}|${env('COMPUTE_EMPTY_VALUE') ?? 'missing'}|${env('COMPUTE_UNDECLARED_VALUE') ?? 'missing'}`);
else if (op === 'artifact') { const out = env('COMPUTE_OUTPUT_DIR'); await writeFile(`${out}/result.json`, '{"ok":true}'); await writeFile(`${out}/report.txt`, 'report'); }
else if (op === 'portable-io') { const data = await readFile(`${env('COMPUTE_WORK_DIR')}/data/input.txt`); if (deno) await Deno.writeFile(`${env('COMPUTE_OUTPUT_DIR')}/data/output.txt`, data); else require('fs').writeFileSync(`${env('COMPUTE_OUTPUT_DIR')}/data/output.txt`, data); }
else if (op === 'large-output') console.log('x'.repeat(8192));
else if (op === 'large-stderr') console.error('x'.repeat(8192));
else if (op === 'sleep') { console.log('started'); await new Promise(resolve => setTimeout(resolve, 5000)); }
})();
"#;

const RUBY_FIXTURE: &str = r##"require 'json'
require 'fileutils'
op = ARGV.shift
case op
when 'args' then puts JSON.generate(ARGV)
when 'stdin' then STDOUT.write(STDIN.read)
when 'streams' then puts "out α\nsecond line with spaces"; warn "err β\nsecond error line"
when 'exit' then puts 'output before exit'; exit ARGV[0].to_i
when 'env' then puts "#{ENV.fetch('COMPUTE_TEST_VALUE', 'missing')}|#{ENV.fetch('COMPUTE_EMPTY_VALUE', 'missing')}|#{ENV.fetch('COMPUTE_UNDECLARED_VALUE', 'missing')}"
when 'artifact'
  out = ENV.fetch('COMPUTE_OUTPUT_DIR'); File.write(File.join(out, 'result.json'), '{"ok":true}'); File.write(File.join(out, 'report.txt'), 'report')
when 'portable-io'
  data = File.binread(File.join(ENV.fetch('COMPUTE_WORK_DIR'), 'data/input.txt')); path = File.join(ENV.fetch('COMPUTE_OUTPUT_DIR'), 'data/output.txt'); FileUtils.mkdir_p(File.dirname(path)); File.binwrite(path, data)
when 'large-output' then STDOUT.write('x' * 8192)
when 'large-stderr' then STDERR.write('x' * 8192)
when 'sleep' then puts 'started'; STDOUT.flush; sleep 5
end
"##;

const PHP_FIXTURE: &str = r#"<?php
$op = $argv[1] ?? '';
$args = array_slice($argv, 2);
if ($op === 'args') echo json_encode($args, JSON_UNESCAPED_UNICODE) . "\n";
elseif ($op === 'stdin') echo stream_get_contents(STDIN);
elseif ($op === 'streams') { echo "out α\nsecond line with spaces\n"; fwrite(STDERR, "err β\nsecond error line\n"); }
elseif ($op === 'exit') { echo "output before exit\n"; exit((int)$args[0]); }
elseif ($op === 'env') echo (getenv('COMPUTE_TEST_VALUE') ?: 'missing') . '|' . (getenv('COMPUTE_EMPTY_VALUE') === false ? 'missing' : getenv('COMPUTE_EMPTY_VALUE')) . '|' . (getenv('COMPUTE_UNDECLARED_VALUE') ?: 'missing') . "\n";
elseif ($op === 'artifact') { $out = getenv('COMPUTE_OUTPUT_DIR'); file_put_contents("$out/result.json", '{"ok":true}'); file_put_contents("$out/report.txt", 'report'); }
elseif ($op === 'portable-io') { $data = file_get_contents(getenv('COMPUTE_WORK_DIR') . '/data/input.txt'); $path = getenv('COMPUTE_OUTPUT_DIR') . '/data/output.txt'; mkdir(dirname($path), 0777, true); file_put_contents($path, $data); }
elseif ($op === 'large-output') echo str_repeat('x', 8192);
elseif ($op === 'large-stderr') fwrite(STDERR, str_repeat('x', 8192));
elseif ($op === 'sleep') { echo "started\n"; flush(); sleep(5); }
?>"#;

const SHELL_FIXTURE: &str = r#"op=$1
shift || true
case "$op" in
  args)
    printf '['; separator=''
    for value in "$@"; do escaped=$(printf '%s' "$value" | sed 's/\\/\\\\/g; s/"/\\"/g'); printf '%s"%s"' "$separator" "$escaped"; separator=','; done
    printf ']\n' ;;
  stdin) cat ;;
  streams) printf 'out α\nsecond line with spaces\n'; printf 'err β\nsecond error line\n' >&2 ;;
  exit) printf 'output before exit\n'; exit "$1" ;;
  env) printf '%s|%s|%s\n' "${COMPUTE_TEST_VALUE-missing}" "${COMPUTE_EMPTY_VALUE-missing}" "${COMPUTE_UNDECLARED_VALUE-missing}" ;;
  artifact) printf '{"ok":true}' > "$COMPUTE_OUTPUT_DIR/result.json"; printf report > "$COMPUTE_OUTPUT_DIR/report.txt" ;;
  portable-io) mkdir -p "$COMPUTE_OUTPUT_DIR/data"; cp "$COMPUTE_WORK_DIR/data/input.txt" "$COMPUTE_OUTPUT_DIR/data/output.txt" ;;
  large-output) head -c 8192 /dev/zero | tr '\000' x ;;
  large-stderr) head -c 8192 /dev/zero | tr '\000' x >&2 ;;
  sleep) printf 'started\n'; sleep 5 ;;
esac
"#;

const DOTNET_FIXTURE: &str = r#"using System.Text.Json;
var argv = Environment.GetCommandLineArgs().Skip(1).ToArray();
var op = argv[0]; var operationArgs = argv.Skip(1).ToArray();
if (op == "args") Console.WriteLine(JsonSerializer.Serialize(operationArgs));
else if (op == "stdin") { using var input = Console.OpenStandardInput(); using var output = Console.OpenStandardOutput(); input.CopyTo(output); }
else if (op == "streams") { Console.Write("out α\nsecond line with spaces\n"); Console.Error.Write("err β\nsecond error line\n"); }
else if (op == "exit") { Console.WriteLine("output before exit"); Environment.Exit(int.Parse(operationArgs[0])); }
else if (op == "env") Console.WriteLine($"{Environment.GetEnvironmentVariable("COMPUTE_TEST_VALUE") ?? "missing"}|{Environment.GetEnvironmentVariable("COMPUTE_EMPTY_VALUE") ?? "missing"}|{Environment.GetEnvironmentVariable("COMPUTE_UNDECLARED_VALUE") ?? "missing"}");
else if (op == "artifact") { var output = Environment.GetEnvironmentVariable("COMPUTE_OUTPUT_DIR")!; File.WriteAllText(Path.Combine(output, "result.json"), "{\"ok\":true}"); File.WriteAllText(Path.Combine(output, "report.txt"), "report"); }
else if (op == "portable-io") { var work = Environment.GetEnvironmentVariable("COMPUTE_WORK_DIR")!; var output = Environment.GetEnvironmentVariable("COMPUTE_OUTPUT_DIR")!; var target = Path.Combine(output, "data/output.txt"); Directory.CreateDirectory(Path.GetDirectoryName(target)!); File.Copy(Path.Combine(work, "data/input.txt"), target); }
else if (op == "large-output") Console.Write(new string('x', 8192));
else if (op == "large-stderr") Console.Error.Write(new string('x', 8192));
else if (op == "sleep") { Console.WriteLine("started"); await Task.Delay(5000); }
"#;

const JAVA_FIXTURE: &str = r#"import java.io.*;
import java.nio.file.*;
import java.util.*;
public class Conformance {
  static String env(String key) { String value = System.getenv(key); return value == null ? "missing" : value; }
  static String json(String[] values) { StringJoiner out = new StringJoiner(",", "[", "]"); for (String value : values) out.add("\"" + value.replace("\\", "\\\\").replace("\"", "\\\"") + "\""); return out.toString(); }
  public static void main(String[] argv) throws Exception {
    String op = argv[0]; String[] args = Arrays.copyOfRange(argv, 1, argv.length);
    if (op.equals("args")) System.out.println(json(args));
    else if (op.equals("stdin")) System.in.transferTo(System.out);
    else if (op.equals("streams")) { System.out.print("out α\nsecond line with spaces\n"); System.err.print("err β\nsecond error line\n"); }
    else if (op.equals("exit")) { System.out.println("output before exit"); System.exit(Integer.parseInt(args[0])); }
    else if (op.equals("env")) System.out.println(env("COMPUTE_TEST_VALUE") + "|" + env("COMPUTE_EMPTY_VALUE") + "|" + env("COMPUTE_UNDECLARED_VALUE"));
    else if (op.equals("artifact")) { Path out = Path.of(env("COMPUTE_OUTPUT_DIR")); Files.writeString(out.resolve("result.json"), "{\"ok\":true}"); Files.writeString(out.resolve("report.txt"), "report"); }
    else if (op.equals("portable-io")) { Path input = Path.of(env("COMPUTE_WORK_DIR"), "data/input.txt"); Path output = Path.of(env("COMPUTE_OUTPUT_DIR"), "data/output.txt"); Files.createDirectories(output.getParent()); Files.copy(input, output); }
    else if (op.equals("large-output")) System.out.print("x".repeat(8192));
    else if (op.equals("large-stderr")) System.err.print("x".repeat(8192));
    else if (op.equals("sleep")) { System.out.println("started"); System.out.flush(); Thread.sleep(5000); }
  }
}
"#;

const NATIVE_FIXTURE: &str = r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static const char *env(const char *key) { const char *value = getenv(key); return value ? value : "missing"; }
static void copy_file(const char *source, const char *target) { FILE *in = fopen(source, "rb"), *out = fopen(target, "wb"); char buffer[4096]; size_t count; while ((count = fread(buffer, 1, sizeof buffer, in))) fwrite(buffer, 1, count, out); fclose(in); fclose(out); }
int main(int argc, char **argv) {
  const char *op = argv[1];
  if (!strcmp(op, "args")) { putchar('['); for (int i = 2; i < argc; i++) { if (i > 2) putchar(','); printf("\"%s\"", argv[i]); } puts("]"); }
  else if (!strcmp(op, "stdin")) { char buffer[4096]; size_t count; while ((count = fread(buffer, 1, sizeof buffer, stdin))) fwrite(buffer, 1, count, stdout); }
  else if (!strcmp(op, "streams")) { fputs("out α\nsecond line with spaces\n", stdout); fputs("err β\nsecond error line\n", stderr); }
  else if (!strcmp(op, "exit")) { puts("output before exit"); return atoi(argv[2]); }
  else if (!strcmp(op, "env")) printf("%s|%s|%s\n", env("COMPUTE_TEST_VALUE"), env("COMPUTE_EMPTY_VALUE"), env("COMPUTE_UNDECLARED_VALUE"));
  else if (!strcmp(op, "artifact")) { char path[4096]; snprintf(path, sizeof path, "%s/result.json", env("COMPUTE_OUTPUT_DIR")); FILE *file = fopen(path, "w"); fputs("{\"ok\":true}", file); fclose(file); snprintf(path, sizeof path, "%s/report.txt", env("COMPUTE_OUTPUT_DIR")); file = fopen(path, "w"); fputs("report", file); fclose(file); }
  else if (!strcmp(op, "portable-io")) { char source[4096], target[4096]; snprintf(source, sizeof source, "%s/data/input.txt", env("COMPUTE_WORK_DIR")); snprintf(target, sizeof target, "%s/data/output.txt", env("COMPUTE_OUTPUT_DIR")); copy_file(source, target); }
  else if (!strcmp(op, "large-output") || !strcmp(op, "large-stderr")) { FILE *out = !strcmp(op, "large-output") ? stdout : stderr; for (int i = 0; i < 8192; i++) fputc('x', out); }
  else if (!strcmp(op, "sleep")) { puts("started"); fflush(stdout); sleep(5); }
  return 0;
}
"#;

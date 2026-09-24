using System.Text.Json;
var argv = Environment.GetCommandLineArgs().Skip(1).ToArray();
var op = argv[0];
if (op == "stdin") { using var input = Console.OpenStandardInput(); using var output = Console.OpenStandardOutput(); input.CopyTo(output); }
else if (op == "exit") Environment.Exit(7);
else if (op == "sleep") await Task.Delay(5000);
else if (op == "certify") {
  string Env(string name) => Environment.GetEnvironmentVariable(name) ?? "missing";
  var result = new { input = File.ReadAllText(Path.Combine(Env("COMPUTE_WORK_DIR"), "hello.txt")), success = true, argument = argv[1], environment = Env("CERTIFICATION_ENV"), host_environment = Env("COMPUTE_HOST_SECRET") };
  File.WriteAllText(Path.Combine(Env("COMPUTE_OUTPUT_DIR"), "result.json"), JsonSerializer.Serialize(result));
  Console.WriteLine(JsonSerializer.Serialize(new { runtime = "dotnet", runtime_version = Environment.Version.ToString() }));
  Console.Error.WriteLine("certification-stderr");
}

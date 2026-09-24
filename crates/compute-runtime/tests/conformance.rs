use compute_runtime_conformance::{FixtureLanguage, run_contract};
use compute_runtime_process::{
    BunRuntime, DenoRuntime, DotnetRuntime, JvmRuntime, NativeRuntime, NodeRuntime, PhpRuntime,
    PythonRuntime, RubyRuntime, ShellRuntime,
};

#[tokio::test]
async fn python_conforms() {
    run_contract(&PythonRuntime, &FixtureLanguage::Python).await;
}

#[tokio::test]
async fn node_conforms() {
    run_contract(&NodeRuntime, &FixtureLanguage::JavaScript).await;
}

#[tokio::test]
async fn bun_conforms_when_installed() {
    run_contract(&BunRuntime, &FixtureLanguage::JavaScript).await;
}

#[tokio::test]
async fn deno_conforms_when_installed() {
    run_contract(&DenoRuntime, &FixtureLanguage::JavaScript).await;
}

#[tokio::test]
async fn ruby_conforms_when_installed() {
    run_contract(&RubyRuntime, &FixtureLanguage::Ruby).await;
}

#[tokio::test]
async fn php_conforms_when_installed() {
    run_contract(&PhpRuntime, &FixtureLanguage::Php).await;
}

#[tokio::test]
async fn shell_conforms_when_installed() {
    run_contract(&ShellRuntime, &FixtureLanguage::Shell).await;
}

#[tokio::test]
async fn jvm_conforms_when_installed() {
    run_contract(&JvmRuntime, &FixtureLanguage::Java).await;
}

#[tokio::test]
async fn dotnet_conforms_when_installed() {
    run_contract(&DotnetRuntime, &FixtureLanguage::Dotnet).await;
}

#[tokio::test]
async fn native_conforms_on_linux() {
    run_contract(&NativeRuntime, &FixtureLanguage::Native).await;
}

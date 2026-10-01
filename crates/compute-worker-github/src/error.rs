use thiserror::Error;

/// Why a runner job did not run. Every message is scrubbed of credentials
/// before the error is constructed.
#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("invalid repository `{0}`: expected OWNER/NAME")]
    InvalidRepository(String),
    #[error("invalid runner specification: {0}")]
    InvalidSpec(String),
    #[error(
        "no GitHub credential: set ${0} to a token that may administer the repository's runners"
    )]
    MissingCredential(String),
    #[error("GitHub could not find repository {0}, or the credential cannot see it")]
    RepositoryNotFound(String),
    #[error("GitHub rejected the credential ({status}): {message}")]
    Unauthorized { status: u16, message: String },
    #[error("GitHub refused the request ({status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("GitHub could not be reached: {0}")]
    Unreachable(String),
    #[error("GitHub's answer was not a registration token")]
    MalformedResponse,
    #[error("the runner execution could not be started: {0}")]
    Execution(String),
}

impl WorkerError {
    /// A stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRepository(_) => "invalid_repository",
            Self::InvalidSpec(_) => "invalid_runner_spec",
            Self::MissingCredential(_) => "missing_credential",
            Self::RepositoryNotFound(_) => "repository_not_found",
            Self::Unauthorized { .. } => "github_unauthorized",
            Self::Rejected { .. } => "github_rejected",
            Self::Unreachable(_) => "github_unreachable",
            Self::MalformedResponse => "github_malformed_response",
            Self::Execution(_) => "runner_execution_failed",
        }
    }
}

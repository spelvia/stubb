use std::path::PathBuf;

/// The command that starts an ACP agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    /// The executable; a bare name is looked up on `PATH`, then through the
    /// user's login shell.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Variables set for the agent, over its [`crate::acp::AcpConfig::environment`].
    pub env: Vec<(String, String)>,
    /// The folder the agent starts in; by default, the session's folder.
    pub cwd: Option<PathBuf>,
}

impl Launch {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    /// Adds arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets a variable for the agent.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        self.env.retain(|(set, _)| *set != name);
        self.env.push((name, value.into()));
        self
    }

    /// Starts the agent in `cwd` rather than in the session's folder.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

use thiserror::Error;

pub type Result<T> = std::result::Result<T, AgentFlowError>;

#[derive(Debug, Error)]
pub enum AgentFlowError {
    #[error("configuration validation failed:\n{0}")]
    ConfigValidation(String),

    #[error("loop error: {0}")]
    Loop(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("render error: {0}")]
    Render(#[from] handlebars::RenderError),
}

impl AgentFlowError {
    pub fn config_validation(errors: Vec<String>) -> Self {
        Self::ConfigValidation(errors.join("\n"))
    }
}

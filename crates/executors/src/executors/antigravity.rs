use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

pub use super::acp::AcpAgentHarness;
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executor_discovery::ExecutorDiscoveredOptions,
    executors::{
        AppendPrompt, AvailabilityInfo, BaseCodingAgent, ExecutorError, SpawnedChild,
        StandardCodingAgentExecutor,
    },
    logs::utils::patch,
    model_selector::{ModelInfo, ModelSelectorConfig, PermissionPolicy},
    profile::ExecutorConfig,
};

const SUPPRESSED_STDERR_PATTERNS: &[&str] = &[
    "was started but never ended. Skipping metrics.",
    "YOLO mode is enabled. All tool calls will be automatically approved.",
];

fn model_info(id: &str, name: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        provider_id: None,
        reasoning_options: vec![],
    }
}

pub(crate) fn parse_agy_models_output(output: &str) -> Vec<ModelInfo> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("Fetching ") {
                return None;
            }
            let (id, name) = line.split_once('\t')?;
            let id = id.trim();
            let name = name.trim();
            if id.is_empty() || name.is_empty() {
                return None;
            }
            Some(model_info(id, name))
        })
        .collect()
}

pub(crate) fn fallback_antigravity_models() -> Vec<ModelInfo> {
    [
        ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
        ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
        ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
        ("gemini-3.7-flash-high", "Gemini 3.7 Flash (High)"),
        ("gemini-3.7-flash-medium", "Gemini 3.7 Flash (Medium)"),
        ("gemini-3.7-flash-low", "Gemini 3.7 Flash (Low)"),
        ("gemini-3.6-flash-high", "Gemini 3.6 Flash (High)"),
        ("gemini-3.6-flash-medium", "Gemini 3.6 Flash (Medium)"),
        ("gemini-3.6-flash-low", "Gemini 3.6 Flash (Low)"),
        ("gemini-3.1-pro-high", "Gemini 3.1 Pro (High)"),
        ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6 (Thinking)"),
        ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
        ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
    ]
    .into_iter()
    .map(|(id, name)| model_info(id, name))
    .collect()
}

async fn discover_antigravity_models() -> Vec<ModelInfo> {
    let agy_binary = if let Ok(path) = which::which("agy") {
        path
    } else if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local").join("bin").join("agy");
        if local_bin.exists() {
            local_bin
        } else {
            std::path::PathBuf::from("agy")
        }
    } else {
        std::path::PathBuf::from("agy")
    };

    let fetch_future = tokio::process::Command::new(&agy_binary)
        .arg("models")
        .output();

    match tokio::time::timeout(Duration::from_secs(3), fetch_future).await {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let models = parse_agy_models_output(&stdout);
            if !models.is_empty() {
                tracing::info!("Discovered {} models from agy", models.len());
                return models;
            }
            tracing::warn!("agy models produced no parsed models, falling back to defaults");
        }
        Ok(Ok(output)) => {
            tracing::warn!(
                "agy models command failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Err(err)) => {
            tracing::warn!("Failed to spawn agy models command: {err}");
        }
        Err(_) => {
            tracing::warn!("Timed out waiting for agy models, using fallback models");
        }
    }

    fallback_antigravity_models()
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct GoogleAntigravity {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl GoogleAntigravity {
    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new("agy");

        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model.as_str()]);
        }

        if self.yolo.unwrap_or(false) {
            builder = builder.extend_params(["--yolo"]);
        }

        builder = builder.extend_params(["--experimental-acp"]);

        apply_overrides(builder, &self.cmd)
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for GoogleAntigravity {
    fn apply_overrides(&mut self, executor_config: &ExecutorConfig) {
        if let Some(model_id) = &executor_config.model_id {
            self.model = Some(model_id.clone());
        }
        if let Some(permission_policy) = executor_config.permission_policy.clone() {
            self.yolo = Some(matches!(
                permission_policy,
                crate::model_selector::PermissionPolicy::Auto
            ));
        }
    }

    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = AcpAgentHarness::new();
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let agy_command = self.build_command_builder()?.build_initial()?;
        let approvals = if self.yolo.unwrap_or(false) {
            None
        } else {
            self.approvals.clone()
        };
        harness
            .spawn_with_command(
                current_dir,
                combined_prompt,
                agy_command,
                env,
                &self.cmd,
                approvals,
            )
            .await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = AcpAgentHarness::new();
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let agy_command = self.build_command_builder()?.build_follow_up(&[])?;
        let approvals = if self.yolo.unwrap_or(false) {
            None
        } else {
            self.approvals.clone()
        };
        harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt,
                session_id,
                agy_command,
                env,
                &self.cmd,
                approvals,
            )
            .await
    }

    fn normalize_logs(
        &self,
        msg_store: Arc<MsgStore>,
        worktree_path: &Path,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        super::acp::normalize_logs_with_suppressed_stderr_patterns(
            msg_store,
            worktree_path,
            SUPPRESSED_STDERR_PATTERNS,
        )
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".antigravity").join("settings.json"))
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if let Some(home) = dirs::home_dir() {
            let agy_local = home.join(".local").join("bin").join("agy");
            let agy_config = home.join(".antigravity");
            if agy_local.exists() || agy_config.exists() {
                return AvailabilityInfo::InstallationFound;
            }
        }

        if which::which("agy").is_ok() {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }

    fn get_preset_options(&self) -> ExecutorConfig {
        use crate::model_selector::*;
        ExecutorConfig {
            executor: BaseCodingAgent::GoogleAntigravity,
            variant: None,
            model_id: self.model.clone(),
            agent_id: None,
            reasoning_id: None,
            permission_policy: Some(if self.yolo.unwrap_or(false) {
                PermissionPolicy::Auto
            } else {
                PermissionPolicy::Supervised
            }),
        }
    }

    async fn discover_options(
        &self,
        _workdir: Option<&std::path::Path>,
        _repo_path: Option<&std::path::Path>,
    ) -> Result<futures::stream::BoxStream<'static, json_patch::Patch>, ExecutorError> {
        let models = discover_antigravity_models().await;
        let default_model = models.first().map(|m| m.id.clone());
        let options = ExecutorDiscoveredOptions {
            model_selector: ModelSelectorConfig {
                models,
                default_model,
                permissions: vec![PermissionPolicy::Auto, PermissionPolicy::Supervised],
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(Box::pin(futures::stream::once(async move {
            patch::executor_discovered_options(options)
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agy_models_tabular_output() {
        let output = "\
gemini-3.8-flash-high\tGemini 3.8 Flash (High)
gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)
claude-opus-4-6-thinking\tClaude Opus 4.6 (Thinking)
";

        let models = parse_agy_models_output(output);

        assert_eq!(models[0].id, "gemini-3.8-flash-high");
        assert_eq!(models[0].name, "Gemini 3.8 Flash (High)");
        assert_eq!(models[1].id, "gemini-3.8-flash-medium");
        assert_eq!(models[2].id, "claude-opus-4-6-thinking");
        assert!(models.iter().all(|m| m.provider_id.is_none()));
    }

    #[test]
    fn ignores_status_lines_and_empty_lines() {
        let output = "\
Fetching available models...

gemini-3.1-pro-low\tGemini 3.1 Pro (Low)
";

        let models = parse_agy_models_output(output);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-3.1-pro-low");
    }

    #[test]
    fn default_model_is_first_fallback_model() {
        let models = fallback_antigravity_models();
        assert_eq!(
            models.first().map(|m| m.id.as_str()),
            Some("gemini-3.8-flash-high")
        );
    }

    #[test]
    fn fallback_models_match_current_agy_choices() {
        let ids: Vec<&str> = fallback_antigravity_models()
            .iter()
            .map(|m| m.id.as_str())
            .collect();

        assert!(ids.contains(&"gemini-3.8-flash-high"));
        assert!(ids.contains(&"gemini-3.7-flash-medium"));
        assert!(ids.contains(&"gemini-3.6-flash-low"));
        assert!(ids.contains(&"gemini-3.1-pro-low"));
        assert!(ids.contains(&"claude-opus-4-6-thinking"));
        assert!(ids.contains(&"gpt-oss-120b-medium"));
    }
}

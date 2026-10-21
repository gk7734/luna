use anyhow::Result;
use async_trait::async_trait;
use collections::HashMap;
use gpui::AsyncApp;
use language::{LanguageName, LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, LanguageServerName, Uri};
use node_runtime::{NodeRuntime, VersionStrategy};
use project::lsp_store::language_server_settings;
use semver::Version;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{ResultExt, maybe};

#[cfg(target_os = "windows")]
const SERVER_PATH: &str =
    "node_modules/@tailwindcss/language-server/bin/tailwindcss-language-server";
#[cfg(not(target_os = "windows"))]
const SERVER_PATH: &str = "node_modules/.bin/tailwindcss-language-server";

fn server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

pub struct TailwindLspAdapter {
    node: NodeRuntime,
}

impl TailwindLspAdapter {
    const SERVER_NAME: LanguageServerName =
        LanguageServerName::new_static("tailwindcss-language-server");
    const PACKAGE_NAME: &str = "@tailwindcss/language-server";

    pub fn new(node: NodeRuntime) -> Self {
        TailwindLspAdapter { node }
    }
}

impl LspInstaller for TailwindLspAdapter {
    type BinaryVersion = Version;

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        self.node
            .npm_package_latest_version(Self::PACKAGE_NAME)
            .await
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate.which(Self::SERVER_NAME.as_ref()).await?;
        let env = delegate.shell_env().await;

        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec!["--stdio".into()],
        })
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        container_dir: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let node = self.node.clone();

        async move {
            let server_path = container_dir.join(SERVER_PATH);

            node.npm_install_latest_packages(&container_dir, &[Self::PACKAGE_NAME])
                .await?;

            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: server_binary_arguments(&server_path),
            })
        }
    }

    fn check_if_version_installed(
        &self,
        version: &Self::BinaryVersion,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let version = version.clone();
        let container_dir = container_dir.clone();

        async move {
            let server_path = container_dir.join(SERVER_PATH);

            let should_install_language_server = node
                .should_install_npm_package(
                    Self::PACKAGE_NAME,
                    &server_path,
                    &container_dir,
                    VersionStrategy::Latest(&version),
                )
                .await;

            if should_install_language_server {
                None
            } else {
                Some(LanguageServerBinary {
                    path: node.binary_path().await.ok()?,
                    env: None,
                    arguments: server_binary_arguments(&server_path),
                })
            }
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_server_binary(container_dir, &self.node).await
    }
}

/// Tailwind is a dependency of the project, or has a (v3) config file. The server otherwise starts
/// for every JavaScript, TypeScript, HTML and CSS project and idles at over 50 MB.
pub(crate) fn project_uses_tailwind(worktree_root: &Path) -> bool {
    const CONFIG_FILES: [&str; 4] = [
        "tailwind.config.js",
        "tailwind.config.cjs",
        "tailwind.config.mjs",
        "tailwind.config.ts",
    ];
    if CONFIG_FILES
        .iter()
        .any(|config_file| worktree_root.join(config_file).is_file())
    {
        return true;
    }
    // A small file read once per project when a server would start.
    std::fs::read_to_string(worktree_root.join("package.json"))
        .is_ok_and(|package_json| package_json.contains("\"tailwindcss\""))
}

#[async_trait(?Send)]
impl LspAdapter for TailwindLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    fn should_start_automatically(&self, worktree_root: &Path) -> bool {
        project_uses_tailwind(worktree_root)
    }

    async fn initialization_options(
        self: Arc<Self>,
        _: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<serde_json::Value>> {
        Ok(Some(json!({
            "provideFormatter": true,
        })))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let mut tailwind_user_settings = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &Self::SERVER_NAME, cx)
                .and_then(|s| s.settings.clone())
                .unwrap_or_default()
        });

        if tailwind_user_settings.get("emmetCompletions").is_none() {
            tailwind_user_settings["emmetCompletions"] = Value::Bool(true);
        }

        if tailwind_user_settings.get("includeLanguages").is_none() {
            tailwind_user_settings["includeLanguages"] = json!({
                "html": "html",
                "css": "css",
                "javascript": "javascript",
                "typescript": "typescript",
                "typescriptreact": "typescriptreact",
            });
        }

        Ok(json!({
            "tailwindCSS": tailwind_user_settings
        }))
    }

    fn language_ids(&self) -> HashMap<LanguageName, String> {
        HashMap::from_iter([
            (LanguageName::new_static("Astro"), "astro".to_string()),
            (LanguageName::new_static("HTML"), "html".to_string()),
            (LanguageName::new_static("Gleam"), "html".to_string()),
            (LanguageName::new_static("CSS"), "css".to_string()),
            (
                LanguageName::new_static("JavaScript"),
                "javascript".to_string(),
            ),
            (
                LanguageName::new_static("TypeScript"),
                "typescript".to_string(),
            ),
            (
                LanguageName::new_static("TSX"),
                "typescriptreact".to_string(),
            ),
            (LanguageName::new_static("Svelte"), "svelte".to_string()),
            (LanguageName::new_static("Elixir"), "elixir".to_string()),
            (LanguageName::new_static("HEEx"), "heex".to_string()),
            (LanguageName::new_static("ERB"), "erb".to_string()),
            (LanguageName::new_static("HTML+ERB"), "erb".to_string()),
            (LanguageName::new_static("PHP"), "php".to_string()),
            (LanguageName::new_static("Vue.js"), "vue".to_string()),
        ])
    }
}

async fn get_cached_server_binary(
    container_dir: PathBuf,
    node: &NodeRuntime,
) -> Option<LanguageServerBinary> {
    maybe!(async {
        let server_path = container_dir.join(SERVER_PATH);
        anyhow::ensure!(
            server_path.exists(),
            "missing executable in directory {server_path:?}"
        );
        Ok(LanguageServerBinary {
            path: node.binary_path().await?,
            env: None,
            arguments: server_binary_arguments(&server_path),
        })
    })
    .await
    .log_err()
}

#[cfg(test)]
mod tests {
    use super::project_uses_tailwind;

    #[test]
    fn test_project_uses_tailwind() {
        let project = tempfile::tempdir().expect("temp dir");
        assert!(!project_uses_tailwind(project.path()));

        std::fs::write(
            project.path().join("package.json"),
            r#"{ "dependencies": { "react": "19.0.0" } }"#,
        )
        .expect("write package.json");
        assert!(!project_uses_tailwind(project.path()));

        std::fs::write(
            project.path().join("package.json"),
            r#"{ "devDependencies": { "tailwindcss": "^4.0.0" } }"#,
        )
        .expect("write package.json");
        assert!(project_uses_tailwind(project.path()));

        let configured_project = tempfile::tempdir().expect("temp dir");
        std::fs::write(configured_project.path().join("tailwind.config.ts"), "")
            .expect("write config");
        assert!(project_uses_tailwind(configured_project.path()));
    }
}

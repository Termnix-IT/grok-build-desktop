//! Shared fixtures for the engine and pipe tests: temporary git repositories, fake agents
//! written as PowerShell scripts, and a sink that records events and log batches in order.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::config::{self, AgentKind, AgentSpec, OutputFormat, PromptVia, Registry};
use super::engine::{
    JobSpec, JobView, LogBatch, Orchestra, OrchestratorEvent, RunStatus, Sink, Source,
};
use super::runner::no_window;

const UTF8_PRELUDE: &str = "$utf8 = New-Object System.Text.UTF8Encoding $false\ntry { [Console]::InputEncoding = $utf8; [Console]::OutputEncoding = $utf8 } catch {}\n";

#[derive(Clone, Debug)]
pub enum Record {
    Event(OrchestratorEvent),
    Logs(LogBatch),
}

#[derive(Default)]
pub struct RecordingSink(pub Mutex<Vec<Record>>);

impl Sink for RecordingSink {
    fn event(&self, event: &OrchestratorEvent) {
        self.0.lock().unwrap().push(Record::Event(event.clone()));
    }
    fn logs(&self, batch: &LogBatch) {
        self.0.lock().unwrap().push(Record::Logs(batch.clone()));
    }
}

pub fn git(dir: &Path, args: &[&str]) -> bool {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    no_window(&mut command);
    command.status().is_ok_and(|status| status.success())
}

/// A repository with one commit holding `tracked.txt`.
pub fn make_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    assert!(git(dir, &["init", "--quiet"]));
    std::fs::write(dir.join("tracked.txt"), "original\n").unwrap();
    assert!(git(dir, &["add", "."]));
    assert!(git(dir, &["commit", "--quiet", "-m", "initial"]));
}

pub struct Kit {
    pub root: PathBuf,
    pub repo: PathBuf,
    pub hub: Orchestra,
    pub sink: Arc<RecordingSink>,
}

impl Kit {
    /// `agents` are (id, PowerShell script). Each script gets the prompt on stdin and runs in
    /// the project folder.
    pub async fn new(agents: &[(&str, &str)], tweak: impl FnOnce(&mut Registry)) -> Self {
        let root = std::env::temp_dir().join(format!("gbd-kit-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        make_repo(&repo);
        let scripts = root.join("agents");
        std::fs::create_dir_all(&scripts).unwrap();
        let mut registry = config::defaults();
        registry.agents = agents
            .iter()
            .map(|(id, script)| {
                let path = scripts.join(format!("{id}.ps1"));
                // A BOM makes Windows PowerShell read the script as UTF-8, and the prelude makes
                // stdin and stdout UTF-8 like the real CLIs (the console default is the OEM page).
                std::fs::write(&path, format!("\u{feff}{UTF8_PRELUDE}{script}")).unwrap();
                fake_agent(id, &path)
            })
            .collect();
        tweak(&mut registry);
        let config_path = root.join("config").join("agents.json");
        config::save(&config_path, &registry).unwrap();
        let sink = Arc::new(RecordingSink::default());
        let hub = Orchestra::new(
            config_path,
            root.join("config").join("gateway.json"),
            root.join("tasks"),
            sink.clone(),
        );
        hub.gateway_save(true, vec![repo.to_string_lossy().into_owned()])
            .unwrap();
        Self {
            root,
            repo,
            hub,
            sink,
        }
    }

    pub fn project(&self) -> String {
        self.repo.to_string_lossy().into_owned()
    }

    pub fn job(&self, agent: &str, prompt: &str) -> JobSpec {
        JobSpec {
            project: self.project(),
            agent: agent.into(),
            prompt: prompt.into(),
            title: None,
            task_id: None,
            timeout_secs: None,
        }
    }

    pub async fn submit(&self, agent: &str, prompt: &str) -> String {
        self.hub
            .submit(self.job(agent, prompt), Source::Bot)
            .await
            .unwrap()
            .job_id
    }

    pub async fn finish(&self, job_id: &str) -> JobView {
        let view = self.hub.wait(job_id, 60).await.unwrap();
        assert!(view.run.status.finished(), "{:?}", view.run);
        view
    }

    pub async fn until_status(&self, job_id: &str, status: RunStatus) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.hub.job(job_id).unwrap().run.status != status {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    /// Waits until the job's log contains `text` (the fake agent printed it).
    pub async fn until_log(&self, job_id: &str, text: &str) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !self.hub.logs(job_id).iter().any(|line| line.text == text) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    pub fn records(&self) -> Vec<Record> {
        self.sink.0.lock().unwrap().clone()
    }
}

impl Drop for Kit {
    fn drop(&mut self) {
        self.hub.shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn fake_agent(id: &str, script: &Path) -> AgentSpec {
    AgentSpec {
        id: id.into(),
        name: id.into(),
        kind: AgentKind::Custom,
        enabled: true,
        program: "powershell".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-File".into(),
            script.to_string_lossy().into_owned(),
        ],
        prompt_via: PromptVia::Stdin,
        prompt_prefix: String::new(),
        output: OutputFormat::Text,
        role: String::new(),
        allow_dangerous: false,
        timeout_secs: None,
    }
}

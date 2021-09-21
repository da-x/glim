use chrono::{DateTime, Local};
use crossterm::{
    event::{Event as CEvent, EventStream, KeyCode},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use futures_timer::Delay;
use gitlab::api::projects::pipelines::PipelineOrderBy;
use gitlab::api::AsyncQuery;
use gitlab::api::{projects, users};
use gitlab::{api::projects::jobs::JobScope, AsyncGitlab, GitlabBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use structopt::StructOpt;
use thiserror::Error;
use tokio::sync::Mutex;
use tui::backend::CrosstermBackend;
use tui::layout::Constraint;
use tui::layout::{Direction, Layout};
use tui::style::Modifier;
use tui::widgets::{Block, Borders, Sparkline};
use tui::widgets::{BorderType, Cell, List, ListItem, Row, Table};
use tui::Terminal;

use masof::keyaction::KeyMap;

mod bridges;
mod util;

use futures::{channel, channel::mpsc, future::FutureExt, select, StreamExt};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Io error; {0}")]
    IoError(#[from] std::io::Error),
    #[error("Gitlab error; {0}")]
    GitlabError(#[from] gitlab::GitlabError),
    // #[error("API error; {0}")]
    // RestError(#[from] gitlab::api::ApiError<gitlab::RestError>),
    #[error("Var error; {0}")]
    VarError(#[from] std::env::VarError),
    #[error("Config error: {0}")]
    ConfigError(#[from] config::ConfigError),
    #[error("Crossterm error: {0}")]
    CrosstermError(#[from] crossterm::ErrorKind),
    #[error("Command error: {0}")]
    BoxError(#[from] Box<dyn std::error::Error + Send>),
    #[error("Serde error: {0}")]
    Serderror(#[from] serde_json::Error),
    #[error("Command error: {0}")]
    Command(String),
    #[error("Query builder error: {0}")]
    BuilderError(String),
    #[error("No pipeline found")]
    NoPipelineFound,
    #[error("No default config file")]
    ConfigFile,
    #[error("Not enough pipelines")]
    NotEnoughPipelines,
}

#[derive(Debug, StructOpt, Clone)]
struct PipelinesMode {
    /// Show the pipelines of all users, not the only the invocing user
    #[structopt(name = "all-users", short = "a")]
    all_users: bool,

    /// Number of pipelines to fetch
    #[structopt(name = "nr-pipelines", short = "n", default_value = "200")]
    nr_pipelines: usize,

    /// Avoid resolving usernames when showing pipelines of all users (slow!)
    #[structopt(name = "usernames-resolve", short = "u")]
    resolve_usernames: bool,

    /// The git branch on which to show pipelines (if not specified - show all refs)
    #[structopt(name = "ref", short = "r")]
    r#ref: Option<String>,
}

#[derive(Debug, StructOpt, Clone)]
struct JobsMode {
    #[structopt(name = "pipeline-id", short = "p")]
    pipeline_id: u64,
}

#[derive(Debug, StructOpt, Clone)]
struct PipeDiff {
    #[structopt(name = "from")]
    from: u64,

    #[structopt(name = "to")]
    to: u64,
}

#[derive(Debug, Clone)]
enum RunMode {
    Pipelines(PipelinesMode),
    PipeDiff(PipeDiff),
    Jobs(JobsMode),
    Help,
    None,
}

#[derive(Debug, StructOpt, Clone)]
struct AliasJobsMode {
    /// Use a specific pipeline number
    #[structopt(name = "pipeline-id", short = "p")]
    pipeline_id: Option<u64>,
}

#[derive(Debug, StructOpt, Clone)]
struct AliasPipelines {
    /// Your pipelines on all your branches instead of current one
    #[structopt(name = "all", short = "a")]
    all_refs: bool,

    /// Show pipeines on specific ref
    #[structopt(name = "ref", short = "r")]
    specific_ref: Option<String>,

    /// Everyone's pipelines
    #[structopt(name = "everyone", short = "e")]
    everyone: bool,
}

#[derive(Debug, StructOpt, Clone)]
enum AliasCommands {
    /// Show all jobs for latest pipeline of the current branch
    #[structopt(name = "jobs")]
    Jobs(AliasJobsMode),

    /// Show all jobs for latest pipeline of the current branch
    #[structopt(name = "pipelines")]
    Pipelines(AliasPipelines),
}

#[derive(Debug, StructOpt, Clone)]
struct TwoPipelinesReports {
    /// Report changes from the last two runs
    #[structopt(name = "ref", short = "r")]
    r#ref: String,

    /// Output file
    #[structopt(name = "output_path", short = "o")]
    out_path: PathBuf,
}

#[derive(Debug, StructOpt, Clone)]
enum ReportCommands {
    /// Show all jobs for latest pipeline of the current branch
    #[structopt(name = "pipediff")]
    TwoPipelinesReports(TwoPipelinesReports),
}

#[derive(Debug, StructOpt, Clone)]
enum CommandMode {
    /// Show all pipelines for a project, or specific to current user
    #[structopt(name = "pipelines")]
    Pipelines(PipelinesMode),

    /// Show difference between two pipes
    #[structopt(name = "pipe-diff")]
    PipeDiff(PipeDiff),

    /// Show all jobs related to a given pipeline
    #[structopt(name = "jobs")]
    Jobs(JobsMode),

    /// Interface for a git aliases that are sensitive to Git's state
    #[structopt(name = "from-alias")]
    FromAlias(AliasCommands),

    /// Generate some non-interactive reports in JSON output
    #[structopt(name = "report")]
    FromReport(ReportCommands),
}

#[derive(Debug, StructOpt, Clone)]
struct CommandArgs {
    #[structopt(name = "config-file", short = "c")]
    config: Option<PathBuf>,

    /// Request debug mode - no TUI
    #[structopt(name = "debug", short = "d")]
    debug: bool,

    /// Non-interactive mode - print data and exit
    #[structopt(name = "non-interactive", short = "n")]
    non_interactive: bool,

    /// Disable auto-refresh (reloading server data)
    #[structopt(name = "disable-auto-refresh", short = "S")]
    disable_auto_refresh: bool,

    #[structopt(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    api_key: String,
    hostname: String,
    project: String,

    /// Path to the local repository clone
    local_repo: Option<PathBuf>,

    /// Webview session can be used for online job traces update
    cookie: Option<String>,

    #[serde(default)]
    hooks: ConfigHooks,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct ConfigHooks {
    /// Shell command to execute when opening a Job (using $SHELL).
    ///
    /// The following environment variables will be defined:
    ///
    /// $GLCIM_JOB_ID, $GLCIM_JOB_NAME, $GLCIM_PIPELINE_ID, $GLCIM_HOSTNAME,
    /// $GLCIM_API_KEY, $GLCIM_PROJECT
    open_job_command: Option<String>,

    /// Shell command to provide the remote ref of the current branch. Defaults
    /// to remote tracking branch via: `git rev-parse --abbrev-ref --symbolic-full-name @{u};`
    remote_ref_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pipeline {
    id: u64,
    status: String,
    r#ref: String,
    sha: String,
    web_url: String,
    created_at: DateTime<Local>,
    updated_at: DateTime<Local>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TwoPipelines {
    r#ref: String,
    from: Pipeline,
    from_jobs: BTreeMap<String, Job>,
    to: Pipeline,
    to_jobs: BTreeMap<String, Job>,
}

#[derive(Debug, Clone, Deserialize)]
struct Bridge {
    downstream_pipeline: Option<Pipeline>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Job {
    id: u64,
    name: String,
    status: String,
    pipeline: Pipeline,
}

use tui::{
    style::{Color, Style},
    text::Span,
};

impl Job {
    fn styled_status(&self) -> Span {
        Span::styled(
            &self.status,
            Style::default().fg(match self.status.as_str() {
                "failed" => Color::Red,
                "running" => Color::Cyan,
                "success" => Color::Green,
                "canceled" => Color::Gray,
                _ => Color::White,
            }),
        )
    }
}

#[derive(Debug, Deserialize)]
struct PipelineDetails {
    id: u64,
    user: User,
}

#[derive(Debug, Deserialize)]
struct User {
    username: String,
}

#[derive(Default)]
struct Updatable<T> {
    update: bool,
    data: Option<(Instant, T)>,
}

impl<T> Updatable<T> {
    fn check_expiry(&mut self, dur: std::time::Duration) -> bool {
        let update = self.update;

        if let Some((last_update, _)) = self.data {
            if last_update.elapsed() >= dur {
                self.update = true;
            }
        } else {
            self.update = true;
        }

        !update && self.update
    }

    fn updated(&mut self) {
        self.update = false;
    }
}

impl<T> Updatable<Vec<T>> {
    fn fix_selected(&self, s: &mut tui::widgets::TableState, visible: Option<usize>) {
        if let Some((_, items)) = &self.data {
            let l = if let Some(visible) = visible {
                std::cmp::min(items.len(), visible)
            } else {
                items.len()
            };

            if let Some(selected) = s.selected() {
                if l > 0 && l <= selected {
                    s.select(Some(l - 1));
                }
            } else {
                if l > 0 {
                    s.select(Some(0));
                } else {
                    s.select(None);
                }
            }
        } else {
            s.select(None);
        }
    }

    fn up(&self, s: &mut tui::widgets::TableState, visible: Option<usize>) {
        if let Some(selected) = s.selected() {
            if selected > 0 {
                s.select(Some(selected - 1));
            }
        } else {
            s.select(Some(0));
        }

        self.fix_selected(s, visible)
    }

    fn down(&self, s: &mut tui::widgets::TableState, visible: Option<usize>) {
        if let Some(selected) = s.selected() {
            s.select(Some(selected + 1));
        } else {
            s.select(Some(0));
        }

        self.fix_selected(s, visible)
    }

    fn home(&self, s: &mut tui::widgets::TableState, visible: Option<usize>) {
        s.select(Some(0));
        self.fix_selected(s, visible)
    }

    fn end(&self, s: &mut tui::widgets::TableState, visible: Option<usize>) {
        s.select(Some(std::usize::MAX));
        self.fix_selected(s, visible)
    }
}

type JobDiff = itertools::EitherOrBoth<(String, Job), (String, Job)>;

#[derive(Default)]
struct State {
    request_count: u64,
    pipelines: Updatable<Vec<Pipeline>>,
    jobs: Updatable<Vec<Job>>,
    pipediff: Updatable<Vec<JobDiff>>,
    pipeline_trigger: std::collections::HashMap<u64, String>,
}

enum RxCmd {
    Refresh,
    UpdateMode(RunMode),
}

struct Thread {
    gitlab: AsyncGitlab,
    rx_cmd: mpsc::Receiver<RxCmd>,
    pipeline_without_trigger: VecDeque<u64>,
    state: Arc<Mutex<State>>,
    rsp_sender: channel::mpsc::Sender<()>,
    current_user: User,
    mode: RunMode,
    config: Config,
    debug: bool,
}

impl Thread {
    async fn get_jobs(
        pipeline_id: u64,
        config: &Config,
        gitlab: &AsyncGitlab,
    ) -> Result<BTreeMap<String, Job>, Error> {
        let mut next_pipeline_id = Some(pipeline_id);
        let mut jobs = vec![];

        while let Some(pipeline_id) = next_pipeline_id.take() {
            let endpoint = projects::pipelines::PipelineJobs::builder()
                .project(config.project.clone())
                .pipeline(pipeline_id)
                .scopes(
                    vec![
                        JobScope::Pending,
                        JobScope::Running,
                        JobScope::Failed,
                        JobScope::Success,
                        JobScope::Canceled,
                    ]
                    .into_iter(),
                )
                .build()
                .map_err(Error::BuilderError)?;
            let endpoint = gitlab::api::paged(endpoint, gitlab::api::Pagination::Limit(300));
            let jobs_query = endpoint.query_async(gitlab);

            let gitlab = gitlab.clone();
            let endpoint = bridges::PipelineBridges::builder()
                .project(config.project.clone())
                .pipeline(pipeline_id)
                .build()
                .map_err(Error::BuilderError)?;
            let endpoint = gitlab::api::paged(endpoint, gitlab::api::Pagination::Limit(3));
            let bridges_query = endpoint.query_async(&gitlab);

            let (jobs_result, bridges_result) = futures::join!(jobs_query, bridges_query);

            let added_jobs: Vec<Job> = jobs_result.map_err(|x| Error::BoxError(Box::new(x)))?;
            let bridges: Vec<Bridge> = bridges_result.map_err(|x| Error::BoxError(Box::new(x)))?;

            jobs.extend(added_jobs);
            for bridge in bridges.into_iter() {
                if let Some(downstream_pipeline) = bridge.downstream_pipeline {
                    next_pipeline_id = Some(downstream_pipeline.id);
                    break;
                }
            }
        }

        let mut map = std::collections::BTreeMap::new();
        for job in jobs.into_iter() {
            map.insert(job.name.clone(), job);
        }

        Ok(map)
    }

    async fn run(&mut self) -> Result<(), Error> {
        loop {
            let mut updates: usize = 0;
            match &self.mode {
                RunMode::None => {
                    return Ok(());
                }
                RunMode::Help => {
                    return Ok(());
                }
                RunMode::Jobs { .. } => {}
                RunMode::PipeDiff { .. } => {}
                RunMode::Pipelines(info) => {
                    if info.resolve_usernames {
                        if let Some(id) = self.pipeline_without_trigger.pop_back() {
                            let endpoint = projects::pipelines::Pipeline::builder()
                                .project(self.config.project.clone())
                                .pipeline(id)
                                .build()
                                .map_err(Error::BuilderError)?;
                            let pipeline: PipelineDetails = endpoint
                                .query_async(&self.gitlab)
                                .await
                                .map_err(|x| Error::BoxError(Box::new(x)))?;
                            let mut state = self.state.lock().await;
                            state.pipeline_trigger.insert(id, pipeline.user.username);
                            state.request_count += 1;
                            updates += 1;
                        }
                    }
                }
            }
            if updates > 0 {
                let _ = self.rsp_sender.try_send(());
            }

            let delay = Delay::new(Duration::from_millis(1000));
            select! {
                _ = delay.fuse() => {
                    continue;
                },
                cmd = self.rx_cmd.next() => {
                    match cmd {
                        Some(RxCmd::Refresh) => {}
                        Some(RxCmd::UpdateMode(mode)) => {
                            self.mode = mode;
                        }
                        _ => return Ok(()),
                    }
                }
            }

            let mut updates: usize = 0;

            if self.debug {
                println!("glim: request iteration");
            }

            match &self.mode {
                RunMode::None => {}
                RunMode::Help => {}
                RunMode::Jobs(info) => {
                    let mut jobs = vec![];

                    if self.state.lock().await.jobs.update {
                        jobs = Self::get_jobs(info.pipeline_id, &self.config, &self.gitlab)
                            .await?
                            .values()
                            .into_iter()
                            .map(|x| x.clone())
                            .collect();
                    }

                    let mut state = self.state.lock().await;
                    if state.jobs.update {
                        state.jobs.data = Some((Instant::now(), jobs));
                        state.request_count += 1;
                        updates += 1;
                        state.jobs.updated();
                    }
                }
                RunMode::PipeDiff(pipediff) => {
                    if self.state.lock().await.pipediff.update {
                        let from = Self::get_jobs(pipediff.from, &self.config, &self.gitlab);
                        let gitlab = self.gitlab.clone();
                        let to = Self::get_jobs(pipediff.to, &self.config, &gitlab);
                        let (from, to) = futures::join!(from, to);
                        let from = from?;
                        let to = to?;

                        let v: Vec<itertools::EitherOrBoth<(String, Job), (String, Job)>> =
                            itertools::merge_join_by(from, to, |(k1, _), (k2, _)| Ord::cmp(k1, k2))
                                .collect();

                        let mut state = self.state.lock().await;
                        if state.pipediff.update {
                            state.pipediff.data = Some((Instant::now(), v));
                            state.request_count += 1;
                            updates += 1;
                            state.pipediff.updated();
                        }
                    }
                }
                RunMode::Pipelines(info) => {
                    if self.state.lock().await.pipelines.update {
                        let mut endpoint = projects::pipelines::Pipelines::builder();
                        endpoint.project(self.config.project.clone());
                        endpoint.order_by(PipelineOrderBy::Id);
                        if !info.all_users {
                            endpoint.username(&self.current_user.username);
                        }
                        if let Some(r#ref) = &info.r#ref {
                            endpoint.ref_(r#ref.to_owned());
                        }
                        let endpoint = endpoint.build().map_err(Error::BuilderError)?;
                        let endpoint = gitlab::api::paged(
                            endpoint,
                            gitlab::api::Pagination::Limit(info.nr_pipelines),
                        );

                        let pipelines: Vec<Pipeline> = endpoint
                            .query_async(&self.gitlab)
                            .await
                            .map_err(|x| Error::BoxError(Box::new(x)))?;

                        if self.debug {
                            println!("glim: pipelines: {:?}", pipelines);
                        }

                        let mut state = self.state.lock().await;

                        for pipeline in pipelines.iter() {
                            if state.pipeline_trigger.get(&pipeline.id).is_none() {
                                self.pipeline_without_trigger.push_front(pipeline.id);
                            }
                        }

                        if state.pipelines.update {
                            state.pipelines.data = Some((Instant::now(), pipelines));
                            state.request_count += 1;
                            updates += 1;
                            state.pipelines.updated();
                        }
                    }
                }
            }

            if updates > 0 {
                let _ = self.rsp_sender.try_send(());
            }
        }
    }
}

#[derive(Ord, PartialOrd, Eq, PartialEq)]
enum Action {
    Quit,
    QuitOrGoBack,
    Clear,
    Diff,
    RefreshToggle,
    HideUnchanged,
    Up,
    Down,
    Help,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    SetFromDiff,
    SetToDiff,
    GoBack,
    Enter,
    OpenInBrowser,
    OpenPreviousInBrowser,
    GitLog,
    ToggleUsernameResolve,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let s = match self {
            Action::Quit => "Quit program",
            Action::QuitOrGoBack => "Go to previous mode or quit",
            Action::Clear => "Clear hidden items",
            Action::Diff => "Diff selected pipelines",
            Action::RefreshToggle => "Toggle auto-refresh",
            Action::HideUnchanged => "In pipediff: toggle visibility of unchanged items",
            Action::Up => "Up one item",
            Action::Down => "Down one item",
            Action::Help => "Help screen",
            Action::PageUp => "Page up in list",
            Action::PageDown => "Page down in list",
            Action::Home => "Go to beginning of list",
            Action::End => "Go to end of list",
            Action::Delete => "Hide list item",
            Action::SetFromDiff => "Set 'from' pipe for diff",
            Action::SetToDiff => "Set 'to' pipe for diff",
            Action::GoBack => "Go back to previous screen",
            Action::Enter => "Enter pipeline or job log",
            Action::OpenInBrowser => "Open job or pipeline in browser",
            Action::OpenPreviousInBrowser => "In pipediff, open previous job in browser",
            Action::GitLog => "Open git commit",
            Action::ToggleUsernameResolve => {
                "Toggle username load when listing other users pipelines"
            }
        };
        write!(f, "{}", s)?;
        Ok(())
    }
}

struct Main {
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,
    state: Arc<Mutex<State>>,
    tx_cmd: mpsc::Sender<RxCmd>,
    rsp_recv: Option<channel::mpsc::Receiver<()>>,
    prev_mode: Option<RunMode>,
    mode: RunMode,
    selected_pipeline: tui::widgets::TableState,
    selected_job: tui::widgets::TableState,
    selected_pipediff: tui::widgets::TableState,
    help: util::StatefulList<String>,
    key_map: KeyMap<Action>,
    ignored_pipeline_ids: std::collections::HashSet<u64>,
    pipelines: Vec<Pipeline>,
    jobs: Vec<Job>,
    pipediffs: Vec<JobDiff>,
    config: Config,
    opt: CommandArgs,
    debug: bool,
    leave: bool,
    pipediff_hide_unchanged: bool,
    pipelines_select_for_diff: Option<(Option<u64>, Option<u64>)>,
    non_interactive: bool,
    auto_refresh: bool,
    first_load: bool,
}
impl Main {
    fn get_remote_branch(config: &Config) -> Result<String, Error> {
        let shell = std::env::var("SHELL")?;
        let mut command = std::process::Command::new(shell);
        command.stdout(std::process::Stdio::piped());
        command.arg("-c");
        let script = if let Some(v) = &config.hooks.remote_ref_command {
            v.as_str()
        } else {
            "git rev-parse --abbrev-ref --symbolic-full-name @{u}"
        };
        command.arg(script);
        let child = command.spawn()?;
        let output = child.wait_with_output()?;
        let s = String::from_utf8_lossy(&output.stdout);
        Ok(s.trim().to_owned())
    }

    async fn command_mode_to_run_mode(
        client: &AsyncGitlab,
        mode: CommandMode,
        config: &Config,
    ) -> Result<RunMode, Error> {
        let alias = match mode {
            CommandMode::Pipelines(x) => return Ok(RunMode::Pipelines(x)),
            CommandMode::PipeDiff(x) => return Ok(RunMode::PipeDiff(x)),
            CommandMode::Jobs(x) => return Ok(RunMode::Jobs(x)),
            CommandMode::FromAlias(alias) => alias,
            CommandMode::FromReport(info) => {
                match info {
                    ReportCommands::TwoPipelinesReports(info) => {
                        let mut endpoint = projects::pipelines::Pipelines::builder();
                        endpoint.project(config.project.clone());
                        endpoint.order_by(PipelineOrderBy::Id);
                        endpoint.ref_(info.r#ref.to_owned());
                        let endpoint = endpoint.build().map_err(Error::BuilderError)?;
                        let endpoint =
                            gitlab::api::paged(endpoint, gitlab::api::Pagination::Limit(2));
                        let pipelines: Vec<Pipeline> = endpoint
                            .query_async(&*client)
                            .await
                            .map_err(|x| Error::BoxError(Box::new(x)))?;
                        if pipelines.len() < 2 {
                            return Err(Error::NotEnoughPipelines);
                        }

                        let to = &pipelines[0];
                        let from = &pipelines[1];

                        let client2 = client.clone();
                        let from_jobs = Thread::get_jobs(from.id, &config, &client2);
                        let to_jobs = Thread::get_jobs(to.id, &config, &client);
                        let (from_jobs, to_jobs) = futures::join!(from_jobs, to_jobs);
                        let from_jobs = from_jobs?;
                        let to_jobs = to_jobs?;

                        use std::fs::OpenOptions;
                        use std::io::{BufWriter, Write};

                        let file = OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(true)
                            .open(info.out_path)?;
                        let mut file = BufWriter::new(file);

                        let two = TwoPipelines {
                            r#ref: info.r#ref.clone(),
                            to: to.clone(),
                            from: from.clone(),
                            to_jobs,
                            from_jobs,
                        };

                        writeln!(&mut file, "{}", serde_json::ser::to_string(&two)?)?;
                    }
                }
                return Ok(RunMode::None);
            }
        };

        match alias {
            AliasCommands::Jobs(info) => {
                Ok(Self::alias_job(info, config, client).await?)
            }
            AliasCommands::Pipelines(info) => {
                let nr_pipelines = 50;

                let branch = if let Some(ref_name) = info.specific_ref {
                    ref_name.clone()
                } else {
                    Self::get_remote_branch(config)?
                };

                if info.everyone {
                    return Ok(RunMode::Pipelines(PipelinesMode {
                        all_users: true,
                        nr_pipelines,
                        resolve_usernames: true,
                        r#ref: None,
                    }));
                }

                if !info.all_refs {
                    if branch.len() > 0 {
                        return Ok(RunMode::Pipelines(PipelinesMode {
                            all_users: true,
                            nr_pipelines,
                            resolve_usernames: false,
                            r#ref: Some(branch),
                        }));
                    }
                }

                return Ok(RunMode::Pipelines(PipelinesMode {
                    all_users: false,
                    nr_pipelines,
                    resolve_usernames: false,
                    r#ref: None,
                }));
            }
        }
    }

    async fn new(opt: &CommandArgs) -> Result<Self, Error> {
        let mut opt = (*opt).clone();
        let config_path = if let Some(config) = &opt.config {
            config.clone()
        } else {
            if let Some(dir) = dirs::config_dir() {
                dir.join("glim").join("config.toml")
            } else {
                return Err(Error::ConfigFile);
            }
        };

        let mut settings = config::Config::default();
        settings
            .merge(config::File::new(config_path.to_str()
                    .ok_or_else(|| Error::ConfigFile)?,
                    config::FileFormat::Toml))?
            // Add in settings from the environment (with a prefix of APP)
            // Eg.. `APP_DEBUG=1 ./target/app` would set the `debug` key
            .merge(config::Environment::with_prefix("GITLAB_CI_CONSOLE"))?;

        // Print out our settings (as a HashMap)
        let config = settings.try_into::<Config>()?;
        let config2 = config.clone();

        let builder = GitlabBuilder::new(&config.hostname, &config.api_key);

        // Create the client.
        let client = builder.build_async().await?;
        let endpoint = users::CurrentUser::builder()
            .build()
            .map_err(Error::BuilderError)?;
        let current_user: User = endpoint
            .query_async(&client)
            .await
            .map_err(|x| Error::BoxError(Box::new(x)))?;

        let state = Arc::new(Mutex::new(State::default()));
        let state2 = state.clone();
        let (tx, rx) = mpsc::channel(10);
        let (rsp_sender, rsp_recv) = channel::mpsc::channel(4);

        let debug = opt.debug;

        let mode = Self::command_mode_to_run_mode(&client, opt.command.clone(), &config).await?;
        let mode2 = mode.clone();

        tokio::spawn(async move {
            let _r = Thread {
                config: config2,
                gitlab: client,
                pipeline_without_trigger: VecDeque::new(),
                state: state2,
                rx_cmd: rx,
                rsp_sender,
                current_user,
                debug,
                mode: mode2,
            }
            .run()
            .await;
        });

        if let RunMode::None = mode {
            opt.non_interactive = true;
        };

        if !opt.debug && !opt.non_interactive {
            enable_raw_mode()?;
        }
        let mut stdout = stdout();
        let terminal = if !opt.debug && !opt.non_interactive {
            execute!(stdout, EnterAlternateScreen)?;
            let backend = CrosstermBackend::new(stdout);
            Some(Terminal::new(backend)?)
        } else {
            None
        };

        Ok(Self {
            terminal,
            state,
            selected_pipeline: Default::default(),
            selected_job: Default::default(),
            selected_pipediff: Default::default(),
            ignored_pipeline_ids: Default::default(),
            pipelines: vec![],
            jobs: vec![],
            pipediffs: vec![],
            leave: false,
            non_interactive: opt.non_interactive,
            tx_cmd: tx,
            config,
            debug: opt.debug,
            pipediff_hide_unchanged: true,
            pipelines_select_for_diff: None,
            auto_refresh: !opt.disable_auto_refresh,
            first_load: true,
            prev_mode: None,
            key_map: KeyMap::new(),
            help: util::StatefulList::new(),
            mode,
            opt,
            rsp_recv: Some(rsp_recv),
        })
    }

    async fn run(&mut self) -> Result<(), Error> {
        match self.mode {
            RunMode::None => {
                return Ok(());
            }
            _ => {}
        }

        self.key_map.add_no_mods(KeyCode::F(1), Action::Help);
        self.key_map.add_no_mods(KeyCode::Up, Action::Up);
        self.key_map.add_no_mods(KeyCode::Down, Action::Down);
        self.key_map
            .add_no_mods(KeyCode::Char('q'), Action::QuitOrGoBack);
        self.key_map.add_no_mods(KeyCode::Char('c'), Action::Clear);
        self.key_map.add_no_mods(KeyCode::Char('d'), Action::Diff);
        self.key_map
            .add_no_mods(KeyCode::Char('r'), Action::RefreshToggle);
        self.key_map
            .add_no_mods(KeyCode::Char('h'), Action::HideUnchanged);
        self.key_map.add_no_mods(KeyCode::PageUp, Action::PageUp);
        self.key_map
            .add_no_mods(KeyCode::PageDown, Action::PageDown);
        self.key_map.add_no_mods(KeyCode::Home, Action::Home);
        self.key_map.add_no_mods(KeyCode::End, Action::End);
        self.key_map.add_no_mods(KeyCode::Delete, Action::Delete);
        self.key_map
            .add_no_mods(KeyCode::Char('f'), Action::SetFromDiff);
        self.key_map
            .add_no_mods(KeyCode::Char('t'), Action::SetToDiff);
        self.key_map.add_no_mods(KeyCode::Backspace, Action::GoBack);
        self.key_map.add_no_mods(KeyCode::Esc, Action::GoBack);
        self.key_map.add_no_mods(KeyCode::Enter, Action::Enter);
        self.key_map
            .add_no_mods(KeyCode::Char('o'), Action::OpenInBrowser);
        self.key_map
            .add_no_mods(KeyCode::Char('p'), Action::OpenPreviousInBrowser);
        self.key_map.add_no_mods(KeyCode::Char('l'), Action::GitLog);
        self.key_map
            .add_no_mods(KeyCode::Char('u'), Action::ToggleUsernameResolve);
        self.key_map.add_ctrl(KeyCode::Char('c'), Action::Quit);

        self.help = util::StatefulList::with_items({
            let mut s = String::new();
            self.key_map.describe(&mut s);
            s.lines().map(|x| x.to_owned()).collect()
        });

        let mut reader = EventStream::new();
        let mut rsp_recv = self.rsp_recv.take().unwrap();

        while !self.leave {
            let updates: usize = if self.auto_refresh || self.first_load {
                self.first_load = false;
                let mut state = self.state.lock().await;
                let mut updates = 0;
                match self.mode {
                    RunMode::None => {
                        break;
                    }
                    RunMode::Help => {}
                    RunMode::Pipelines { .. } => {
                        if state
                            .pipelines
                            .check_expiry(std::time::Duration::from_millis(10000))
                        {
                            updates += 1;
                        }
                    }
                    RunMode::Jobs { .. } => {
                        if state
                            .jobs
                            .check_expiry(std::time::Duration::from_millis(10000))
                        {
                            updates += 1;
                        }
                    }
                    RunMode::PipeDiff { .. } => {
                        if state
                            .pipediff
                            .check_expiry(std::time::Duration::from_millis(60000))
                        {
                            updates += 1;
                        }
                    }
                }
                updates
            } else {
                0
            };
            if updates > 0 {
                let _ = self.tx_cmd.try_send(RxCmd::Refresh);
            }

            if !self.debug {
                if !self.non_interactive {
                    self.draw().await?;
                } else {
                    if self.check_report_non_interactive().await? {
                        break;
                    }
                }
            }

            let delay = Delay::new(Duration::from_millis(1000));

            select! {
                _ = delay.fuse() => {  },
                maybe_event = reader.next().fuse() => {
                    match maybe_event {
                        Some(Ok(CEvent::Mouse{..})) => continue,
                        Some(Ok(event)) => { self.on_event(event).await? }
                        Some(Err(_)) => {
                            break;
                        }
                        None => {}
                    }
                }
                msg = rsp_recv.next() => {
                    match msg {
                        Some(()) => {},
                        None => {},
                    }
                }
            }
        }

        Ok(())
    }

    async fn draw(&mut self) -> Result<(), Error> {
        match &self.mode {
            RunMode::None => Ok(()),
            RunMode::PipeDiff(info) => {
                let info = info.clone();
                self.draw_pipediff(info).await
            }
            RunMode::Pipelines(info) => {
                let info = info.clone();
                self.draw_pipelines(info).await
            }
            RunMode::Help => self.draw_help().await,
            RunMode::Jobs { .. } => self.draw_jobs().await,
        }
    }

    async fn check_report_non_interactive(&mut self) -> Result<bool, Error> {
        match &self.mode {
            RunMode::PipeDiff { .. } => {
                return self.non_interactive_pipediff().await;
            }
            _ => {}
        };

        Ok(false)
    }

    async fn non_interactive_pipediff(&mut self) -> Result<bool, Error> {
        let state = self.state.lock().await;

        if let Some((_, pipediff)) = &state.pipediff.data {
            for res in pipediff.iter() {
                match res {
                    itertools::EitherOrBoth::Both((k, a), (_, b)) => {
                        if a.status != b.status {
                            println!("{:width$}: {} -> {}", k, a.status, b.status, width = 40);
                        }
                    }
                    _ => {}
                }
            }
            return Ok(true);
        }

        Ok(false)
    }

    fn status_line(auto_refresh: bool) -> Sparkline<'static> {
        Sparkline::default()
            .block(Block::default().title({
                let mut s = String::new();
                if auto_refresh {
                    s += "F1 for help | Auto-refresh: enabled";
                } else {
                    s += "F1 for help | Auto-refresh: disabled";
                }
                s
            }))
            .data(&[0])
            .style(Style::default().fg(Color::White))
    }

    fn divide_for_status_line(rect: tui::layout::Rect) -> Vec<tui::layout::Rect> {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Max(1)].as_ref())
            .split(rect)
    }

    async fn draw_pipelines(&mut self, info: PipelinesMode) -> Result<(), Error> {
        let state = self.state.lock().await;

        state
            .pipelines
            .fix_selected(&mut self.selected_pipeline, Some(self.pipelines.len()));
        let selected_pipeline = &mut self.selected_pipeline;
        let pipeline_ids = &mut self.pipelines;
        let ignored_pipeline_ids = &self.ignored_pipeline_ids;
        let sparkline = Self::status_line(self.auto_refresh);
        let pipelines_select_for_diff = &self.pipelines_select_for_diff;

        self.terminal.as_mut().unwrap().draw(|rect| {
            let mut items: Vec<_> = vec![];
            pipeline_ids.clear();
            let mut widths = vec![Constraint::Length(8)];
            if info.resolve_usernames {
                widths.push(Constraint::Length(15));
            };

            if pipelines_select_for_diff.is_some() {
                widths.push(Constraint::Length(5));
            }

            widths.extend(vec![
                Constraint::Length(10),
                Constraint::Length(30),
                Constraint::Length(12),
                Constraint::Length(19),
            ]);

            if let Some((_, pipelines)) = &state.pipelines.data {
                for pipeline in pipelines.iter() {
                    if ignored_pipeline_ids.get(&pipeline.id).is_some() {
                        continue;
                    }

                    pipeline_ids.push((*pipeline).clone());
                    let user = if let Some(user) = state.pipeline_trigger.get(&pipeline.id) {
                        user
                    } else {
                        "..."
                    };

                    let status = Span::styled(
                        &pipeline.status,
                        Style::default().fg(match pipeline.status.as_str() {
                            "failed" => Color::Red,
                            "running" => Color::Cyan,
                            "success" => Color::Green,
                            "canceled" => Color::Gray,
                            _ => Color::White,
                        }),
                    );

                    let mut v = vec![Cell::from(Span::raw(pipeline.id.to_string()))];

                    if info.resolve_usernames {
                        v.push(Cell::from(Span::raw(user)));
                    }

                    if let Some((from, to)) = pipelines_select_for_diff {
                        let mut f = Span::raw(" ");
                        if let Some(from) = from {
                            if pipeline.id == *from {
                                f = Span::raw("from");
                            }
                        }
                        if let Some(to) = to {
                            if pipeline.id == *to {
                                f = Span::raw("to");
                            }
                        }
                        v.push(Cell::from(f));
                    }

                    v.extend(vec![
                        Cell::from(status),
                        Cell::from(Span::raw(pipeline.r#ref.to_string())),
                        Cell::from(Span::raw(pipeline.sha.to_string())),
                        Cell::from(Span::raw(pipeline.created_at.to_string())),
                    ]);

                    items.push(Row::new(v));
                }
            }

            let pipelines = Table::new(items)
                .header(Row::new({
                    let mut v = vec![Cell::from(Span::styled(
                        "ID",
                        Style::default().add_modifier(Modifier::BOLD),
                    ))];

                    if info.resolve_usernames {
                        v.push(Cell::from(Span::styled(
                            "User",
                            Style::default().add_modifier(Modifier::BOLD),
                        )));
                    }

                    if pipelines_select_for_diff.is_some() {
                        v.push(Cell::from(Span::styled(
                            "Diff",
                            Style::default().add_modifier(Modifier::BOLD),
                        )));
                    }

                    v.extend(
                        [
                            Cell::from(Span::styled(
                                "Status",
                                Style::default().add_modifier(Modifier::BOLD),
                            )),
                            Cell::from(Span::styled(
                                "Ref",
                                Style::default().add_modifier(Modifier::BOLD),
                            )),
                            Cell::from(Span::styled(
                                "GitHash",
                                Style::default().add_modifier(Modifier::BOLD),
                            )),
                            Cell::from(Span::styled(
                                "Created at",
                                Style::default().add_modifier(Modifier::BOLD),
                            )),
                        ]
                        .iter()
                        .cloned(),
                    );
                    v
                }))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .style(Style::default().fg(Color::White))
                        .title("Pipelines")
                        .border_type(BorderType::Plain),
                )
                .highlight_style(Style::default().bg(Color::Rgb(60, 60, 80)))
                .widths(&widths);

            let chunks = Self::divide_for_status_line(rect.size());
            rect.render_stateful_widget(pipelines, chunks[0], selected_pipeline);
            rect.render_widget(sparkline, chunks[1]);
        })?;

        Ok(())
    }

    async fn draw_jobs(&mut self) -> Result<(), Error> {
        let sparkline = Self::status_line(self.auto_refresh);
        let state = self.state.lock().await;

        state.jobs.fix_selected(&mut self.selected_job, None);
        let printed_jobs = &mut self.jobs;
        let selected_job = &mut self.selected_job;

        printed_jobs.clear();

        self.terminal.as_mut().unwrap().draw(|rect| {
            let mut items: Vec<_> = vec![];
            if let Some((_, jobs)) = &state.jobs.data {
                for job in jobs.iter() {
                    printed_jobs.push((*job).clone());
                    let status = job.styled_status();
                    items.push(Row::new(vec![
                        Cell::from(Span::raw(job.id.to_string())),
                        Cell::from(Span::raw(job.pipeline.id.to_string())),
                        Cell::from(status),
                        Cell::from(Span::raw(job.name.to_string())),
                    ]));
                }
            }

            let jobs = Table::new(items)
                .header(Row::new(vec![
                    Cell::from(Span::styled(
                        "ID",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        "Pipeline",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        "Status",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        "Name",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .style(Style::default().fg(Color::White))
                        .title("Jobs")
                        .border_type(BorderType::Plain),
                )
                .highlight_style(Style::default().bg(Color::Rgb(60, 60, 80)))
                .widths(&[
                    Constraint::Length(9),
                    Constraint::Length(9),
                    Constraint::Length(10),
                    Constraint::Length(40),
                ]);

            let chunks = Self::divide_for_status_line(rect.size());
            rect.render_stateful_widget(jobs, chunks[0], selected_job);
            rect.render_widget(sparkline, chunks[1]);
        })?;

        Ok(())
    }

    async fn draw_help(&mut self) -> Result<(), Error> {
        let help = &mut self.help;

        self.terminal.as_mut().unwrap().draw(|rect| {
            let items: Vec<_> = help
                .items
                .iter()
                .map(|x| ListItem::new(x.clone()))
                .collect();
            let help_list = List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .style(Style::default().fg(Color::White))
                    .title("Help")
                    .border_type(BorderType::Plain),
            );

            let chunks = Self::divide_for_status_line(rect.size());
            rect.render_stateful_widget(help_list, chunks[0], &mut help.state);
        })?;

        Ok(())
    }

    async fn draw_pipediff(&mut self, info: PipeDiff) -> Result<(), Error> {
        let sparkline = Self::status_line(self.auto_refresh);
        let state = self.state.lock().await;

        state.pipediff.fix_selected(&mut self.selected_job, None);
        let printed_pipediffs = &mut self.pipediffs;
        let selected_pipediff = &mut self.selected_pipediff;

        printed_pipediffs.clear();

        let pipediff_hide_unchanged = self.pipediff_hide_unchanged;
        self.terminal.as_mut().unwrap().draw(|rect| {
            let mut items: Vec<_> = vec![];
            if let Some((_, pipediff)) = &state.pipediff.data {
                for pipediff in pipediff.iter() {
                    match pipediff {
                        itertools::EitherOrBoth::Both((k, a), (_, b)) => {
                            if pipediff_hide_unchanged && &a.status == &b.status {
                                continue;
                            }

                            printed_pipediffs.push((*pipediff).clone());
                            items.push(Row::new(vec![
                                Cell::from(Span::raw(k.to_string())),
                                Cell::from(Span::raw(a.id.to_string())),
                                Cell::from(Span::raw(b.id.to_string())),
                                Cell::from(a.styled_status()),
                                Cell::from(b.styled_status()),
                            ]));
                        }
                        _ => {}
                    }
                }
            }

            let pipediffs = Table::new(items)
                .header(Row::new(vec![
                    Cell::from(Span::styled(
                        "Name",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        "Old ID",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        "New ID",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        "Old Status",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        "New Status",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .style(Style::default().fg(Color::White))
                        .title(format!("Jobs: Pipe diff {} -> {}", info.from, info.to))
                        .border_type(BorderType::Plain),
                )
                .highlight_style(Style::default().bg(Color::Rgb(60, 60, 80)))
                .widths(&[
                    Constraint::Length(40),
                    Constraint::Length(12),
                    Constraint::Length(12),
                    Constraint::Length(10),
                    Constraint::Length(10),
                ]);

            let chunks = Self::divide_for_status_line(rect.size());
            rect.render_stateful_widget(pipediffs, chunks[0], selected_pipediff);
            rect.render_widget(sparkline, chunks[1]);
        })?;

        Ok(())
    }

    async fn on_up(&mut self) -> Result<(), Error> {
        let state = self.state.lock().await;

        match self.mode {
            RunMode::Help => self.help.previous(),
            RunMode::None => {}
            RunMode::Jobs(_) => state.jobs.up(&mut self.selected_job, None),
            RunMode::Pipelines(_) => state.pipelines.up(&mut self.selected_pipeline, None),
            RunMode::PipeDiff { .. } => state.pipediff.up(&mut self.selected_pipediff, None),
        }

        Ok(())
    }

    async fn on_down(&mut self) -> Result<(), Error> {
        let state = self.state.lock().await;

        match self.mode {
            RunMode::None => {}
            RunMode::Help => self.help.next(),
            RunMode::Jobs(_) => state
                .jobs
                .down(&mut self.selected_job, Some(self.jobs.len())),
            RunMode::Pipelines(_) => state
                .pipelines
                .down(&mut self.selected_pipeline, Some(self.pipelines.len())),
            RunMode::PipeDiff { .. } => state
                .pipediff
                .down(&mut self.selected_pipediff, Some(self.pipediffs.len())),
        }

        Ok(())
    }

    async fn goto_top_of_list(&mut self) -> Result<(), Error> {
        let state = self.state.lock().await;

        match self.mode {
            RunMode::None => {}
            RunMode::Help => {}
            RunMode::Jobs(_) => state.jobs.home(&mut self.selected_job, None),
            RunMode::Pipelines(_) => state.pipelines.home(&mut self.selected_pipeline, None),
            RunMode::PipeDiff { .. } => state.pipediff.home(&mut self.selected_pipediff, None),
        }

        Ok(())
    }

    async fn goto_end_of_list(&mut self) -> Result<(), Error> {
        let state = self.state.lock().await;

        match self.mode {
            RunMode::Help => {}
            RunMode::None => {}
            RunMode::Jobs(_) => state
                .jobs
                .end(&mut self.selected_job, Some(self.jobs.len())),
            RunMode::Pipelines(_) => state
                .pipelines
                .end(&mut self.selected_pipeline, Some(self.pipelines.len())),
            RunMode::PipeDiff { .. } => state
                .pipediff
                .end(&mut self.selected_pipediff, Some(self.pipediffs.len())),
        }

        Ok(())
    }

    fn go_to_previous_mode(&mut self) -> Result<(), Error> {
        if let Some(mode) = self.prev_mode.take() {
            self.mode = mode;
            self.first_load = true;
            let _ = self.tx_cmd.try_send(RxCmd::UpdateMode(self.mode.clone()));
        }

        Ok(())
    }

    fn on_set_pipediff_range(&mut self, set_from: bool) -> Result<(), Error> {
        if let Some((Some(from), Some(to))) = &mut self.pipelines_select_for_diff {
            if let Some(selected) = self.selected_pipeline.selected() {
                if selected < self.pipelines.len() {
                    let pipeline = &self.pipelines[selected];
                    if set_from {
                        if pipeline.id != *to {
                            *from = pipeline.id;
                        }
                    } else {
                        if pipeline.id != *from {
                            *to = pipeline.id;
                        }
                    }
                }
            }
            return Ok(());
        }

        Ok(())
    }

    fn on_enter_job(&self, job: &Job, pipeline_id: u64) -> Result<(), Error> {
        if let Some(open_job_command) = &self.config.hooks.open_job_command {
            let shell = std::env::var("SHELL")?;
            let mut command = std::process::Command::new(shell);
            command.arg("-c");
            command.arg(open_job_command);
            command.env("GLCIM_JOB_ID", format!("{}", job.id));
            command.env("GLCIM_JOB_NAME", format!("{}", job.name));
            command.env("GLCIM_PIPELINE_ID", format!("{}", pipeline_id));
            command.env("GLCIM_PROJECT", &self.config.project);
            command.env("GLCIM_HOSTNAME", &self.config.hostname);
            command.env("GLCIM_API_KEY", &self.config.api_key);
            if let Some(cookie) = &self.config.cookie {
                command.env("GLCIM_COOKIE", &cookie);
            }
            if let Ok(mut v) = command.spawn() {
                let _ = v.wait();
            }
        }

        Ok(())
    }

    async fn on_enter(&mut self) -> Result<(), Error> {
        match self.mode {
            RunMode::None => {}
            RunMode::Help => {}
            RunMode::Jobs(ref info) => {
                if let Some(selected) = self.selected_job.selected() {
                    if selected < self.jobs.len() {
                        let job = &self.jobs[selected];
                        self.on_enter_job(job, info.pipeline_id)?;
                    }
                }
            }
            RunMode::PipeDiff(ref info) => {
                if let Some(selected) = self.selected_pipediff.selected() {
                    if selected < self.pipediffs.len() {
                        match &self.pipediffs[selected] {
                            itertools::EitherOrBoth::Both(_, (_, b)) => {
                                self.on_enter_job(&b, info.to)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
            RunMode::Pipelines(_) => {
                if let Some((Some(from), Some(to))) = &self.pipelines_select_for_diff {
                    self.prev_mode = Some(std::mem::replace(
                        &mut self.mode,
                        RunMode::PipeDiff(PipeDiff {
                            from: *from,
                            to: *to,
                        }),
                    ));
                    self.selected_pipediff.select(None);
                    self.first_load = true;

                    let mut state = self.state.lock().await;
                    state.pipediff.data = None;
                    let _ = self.tx_cmd.try_send(RxCmd::UpdateMode(self.mode.clone()));
                    return Ok(());
                }

                if let Some(selected) = self.selected_pipeline.selected() {
                    if selected < self.pipelines.len() {
                        let pipeline = &self.pipelines[selected];
                        self.selected_job.select(None);
                        self.prev_mode = Some(std::mem::replace(
                            &mut self.mode,
                            RunMode::Jobs(JobsMode {
                                pipeline_id: pipeline.id,
                            }),
                        ));
                        self.first_load = true;
                        let mut state = self.state.lock().await;
                        state.jobs.data = None;
                        let _ = self.tx_cmd.try_send(RxCmd::UpdateMode(self.mode.clone()));
                    }
                }
            }
        }

        Ok(())
    }

    async fn alias_job(info: AliasJobsMode, config: &Config, client: &AsyncGitlab) -> Result<RunMode, Error> {
        let pipeline_id = if let Some(id) = info.pipeline_id {
            id
        } else {
            let remote_ref = Self::get_remote_branch(config)?;

            let mut endpoint = projects::pipelines::Pipelines::builder();
            endpoint.project(config.project.clone());
            endpoint.order_by(PipelineOrderBy::Id);
            endpoint.ref_(remote_ref.to_owned());
            let endpoint = endpoint.build().map_err(Error::BuilderError)?;
            let endpoint = gitlab::api::paged(endpoint, gitlab::api::Pagination::Limit(1));

            let mut pipelines: Vec<Pipeline> = endpoint
                .query_async(client)
                .await
                .map_err(|x| Error::BoxError(Box::new(x)))?;

            if let Some(pipeline) = pipelines.pop() {
                pipeline.id
            } else {
                return Err(Error::NoPipelineFound);
            }
        };

        Ok(RunMode::Jobs(JobsMode { pipeline_id }))
    }

    fn on_job_open_in_browser(&self, job_id: u64) -> Result<(), Error> {
        let url = format!(
            "https://{}/{}/-/jobs/{}",
            &self.config.hostname, &self.config.project, job_id
        );
        let _ = webbrowser::open(&url);
        Ok(())
    }

    fn on_pipeline_open_in_browser(&self, pipeline_id: u64) -> Result<(), Error> {
        let url = format!(
            "https://{}/{}/pipelines/{}",
            &self.config.hostname, &self.config.project, pipeline_id
        );
        let _ = webbrowser::open(&url);
        Ok(())
    }

    fn open_browser(&mut self) -> Result<(), Error> {
        match self.mode {
            RunMode::None => {}
            RunMode::Help => {}
            RunMode::Jobs(_) => {
                if let Some(selected) = self.selected_job.selected() {
                    if selected < self.jobs.len() {
                        let job = &self.jobs[selected];
                        self.on_job_open_in_browser(job.id)?;
                    }
                }
            }
            RunMode::PipeDiff(_) => {
                if let Some(selected) = self.selected_pipediff.selected() {
                    if selected < self.pipediffs.len() {
                        match &self.pipediffs[selected] {
                            itertools::EitherOrBoth::Both(_, (_, b)) => {
                                self.on_job_open_in_browser(b.id)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
            RunMode::Pipelines(_) => {
                if let Some(selected) = self.selected_pipeline.selected() {
                    if selected < self.pipelines.len() {
                        let pipeline = &self.pipelines[selected];
                        self.on_pipeline_open_in_browser(pipeline.id)?;
                    }
                }
            }
        }

        Ok(())
    }

    fn toggle_username_resolve(&mut self) -> Result<(), Error> {
        match &mut self.mode {
            RunMode::None => {}
            RunMode::Help => {}
            RunMode::Jobs(_) => {}
            RunMode::PipeDiff(_) => {}
            RunMode::Pipelines(info) => {
                info.resolve_usernames = !info.resolve_usernames;
                let _ = self.tx_cmd.try_send(RxCmd::UpdateMode(self.mode.clone()));
            }
        }

        Ok(())
    }

    fn open_previous(&mut self) -> Result<(), Error> {
        match self.mode {
            RunMode::None => {}
            RunMode::Help => {}
            RunMode::Jobs(_) => {}
            RunMode::PipeDiff(ref info) => {
                if let Some(selected) = self.selected_pipediff.selected() {
                    if selected < self.pipediffs.len() {
                        match &self.pipediffs[selected] {
                            itertools::EitherOrBoth::Both((_, a), _) => {
                                self.on_enter_job(&a, info.from)?;
                            }
                            _ => {}
                        }
                    }
                }
            }
            RunMode::Pipelines(_) => {}
        }

        Ok(())
    }

    fn ignore_pipeline(&mut self) -> Result<(), Error> {
        match self.mode {
            RunMode::None => {}
            RunMode::Help => {}
            RunMode::Jobs(_) => {}
            RunMode::PipeDiff { .. } => {}
            RunMode::Pipelines(_) => {
                if let Some(selected) = self.selected_pipeline.selected() {
                    if selected < self.pipelines.len() {
                        let pipeline = &self.pipelines[selected];
                        self.ignored_pipeline_ids.insert(pipeline.id);
                        if selected > 0 && selected + 1 == self.pipelines.len() {
                            self.selected_pipeline.select(Some(selected - 1));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn open_git_log(&mut self) -> Result<(), Error> {
        match self.mode {
            RunMode::None => {}
            RunMode::Help => {}
            RunMode::Jobs(_) => {}
            RunMode::PipeDiff { .. } => {}
            RunMode::Pipelines(_) => {
                if let Some(local_repo) = &self.config.local_repo {
                    if let Some(selected) = self.selected_pipeline.selected() {
                        if selected < self.pipelines.len() {
                            let pipeline = &self.pipelines[selected];
                            let mut command = std::process::Command::new("git");
                            command.arg("log");
                            command.arg(&pipeline.sha);
                            command.current_dir(&local_repo);
                            execute!(stdout(), Clear(ClearType::All))?;
                            self.release_terminal()?;
                            if let Ok(mut v) = command.spawn() {
                                let _ = v.wait();
                            }
                            self.gain_terminal()?;
                            self.terminal.as_mut().unwrap().clear()?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn on_event(&mut self, event: CEvent) -> Result<(), Error> {
        match event {
            CEvent::Key(key_event) => {
                let action = if let Some(action) = self.key_map.get_action(key_event) {
                    action
                } else {
                    return Ok(());
                };

                match action {
                    Action::Help => {
                        self.prev_mode = Some(std::mem::replace(&mut self.mode, RunMode::Help))
                    }
                    Action::Quit => self.leave = true,
                    Action::QuitOrGoBack => {
                        if self.prev_mode.is_some() {
                            self.go_to_previous_mode()?
                        } else {
                            self.leave = true;
                        }
                    }
                    Action::Clear => self.ignored_pipeline_ids.clear(),
                    Action::Diff => {
                        if self.pipelines_select_for_diff.is_some() {
                            self.pipelines_select_for_diff = None;
                        } else {
                            if self.pipelines.len() >= 2 {
                                let from = &self.pipelines[0];
                                let to = &self.pipelines[1];
                                self.pipelines_select_for_diff = Some((Some(to.id), Some(from.id)))
                            }
                        }
                    }
                    Action::RefreshToggle => self.auto_refresh = !self.auto_refresh,
                    Action::HideUnchanged => {
                        self.pipediff_hide_unchanged = !self.pipediff_hide_unchanged
                    }
                    Action::Up => self.on_up().await?,
                    Action::Down => self.on_down().await?,
                    Action::PageUp => {
                        for _ in 0..crossterm::terminal::size()?.1.saturating_sub(2) {
                            self.on_up().await?;
                        }
                    }
                    Action::PageDown => {
                        for _ in 0..crossterm::terminal::size()?.1.saturating_sub(2) {
                            self.on_down().await?;
                        }
                    }
                    Action::Home => self.goto_top_of_list().await?,
                    Action::End => self.goto_end_of_list().await?,
                    Action::Delete => self.ignore_pipeline()?,
                    Action::GoBack => self.go_to_previous_mode()?,
                    Action::SetToDiff => self.on_set_pipediff_range(false)?,
                    Action::SetFromDiff => self.on_set_pipediff_range(true)?,
                    Action::Enter => self.on_enter().await?,
                    Action::OpenInBrowser => self.open_browser()?,
                    Action::OpenPreviousInBrowser => self.open_previous()?,
                    Action::GitLog => self.open_git_log()?,
                    Action::ToggleUsernameResolve => self.toggle_username_resolve()?,
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn gain_terminal(&mut self) -> Result<(), Error> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen)?;
        Ok(())
    }

    fn release_terminal(&mut self) -> Result<(), Error> {
        disable_raw_mode()?;
        execute!(
            self.terminal.as_mut().unwrap().backend_mut(),
            LeaveAlternateScreen,
        )?;
        self.terminal.as_mut().unwrap().show_cursor()?;
        Ok(())
    }
}

fn main_wrap() -> Result<(), Error> {
    let opt = CommandArgs::from_args();

    let (mut glim, v) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(3)
        .build()?
        .block_on(async {
            match Main::new(&opt).await {
                Err(err) => Err(err),
                Ok(mut glim) => {
                    let v = glim.run().await;
                    Ok((glim, v))
                }
            }
        })?;

    if !opt.debug && !glim.opt.non_interactive {
        glim.release_terminal()?;
    }
    v?;

    Ok(())
}

fn main() {
    match main_wrap() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(-1);
        }
    }
}

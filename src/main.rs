use serde::Deserialize;
use gitlab::{Gitlab, api::projects::jobs::JobScope};
use gitlab::api::{projects, Query, users};
use gitlab::api::projects::pipelines::PipelineOrderBy;
use chrono::{DateTime, Local};
use thiserror::Error;
use structopt::StructOpt;
use std::path::PathBuf;
use std::sync::{Mutex, mpsc, Arc};
use std::io::stdout;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event as CEvent, EventStream},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, Clear, EnterAlternateScreen, LeaveAlternateScreen, ClearType},
};
use std::time::{Duration, Instant};
use tui::Terminal;
use tui::backend::CrosstermBackend;
use tui::widgets::{Block, Borders};
use tui::layout::Constraint;
use futures_timer::Delay;

use futures::{
    channel,
    future::FutureExt, select, StreamExt,
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Io error; {0}")]
    IoError(#[from] std::io::Error),
    #[error("Var error; {0}")]
    VarError(#[from] std::env::VarError),
    #[error("Config error: {0}")]
    ConfigError(#[from] config::ConfigError),
    #[error("Recv error: {0}")]
    RecvError(#[from] mpsc::RecvError),
    #[error("Crossterm error: {0}")]
    CrosstermError(#[from] crossterm::ErrorKind),
    #[error("Command error: {0}")]
    Command(String),
    #[error("No default config file")]
    ConfigFile,
}

#[derive(Debug, StructOpt, Clone)]
struct PipelinesMode {
    /// Show the pipelines of all users
    #[structopt(name = "all-users", short="a")]
    all_users: bool,

    /// Number of pipelines to fetch
    #[structopt(name = "nr-pipelines", short="n", default_value="200")]
    nr_pipelines: usize,

    /// Avoid resolving usernames when showing pipelines of all users
    #[structopt(name = "usernames-resolve", short="u")]
    resolve_usernames: bool,

    #[structopt(name = "ref", short="r")]
    r#ref: Option<String>,
}

#[derive(Debug, StructOpt, Clone)]
struct JobsMode {
    #[structopt(name = "pipeline-id", short="p")]
    pipeline_id: u64,
}

#[derive(Debug, StructOpt, Clone)]
struct PipeDiff {
    #[structopt(name = "from")]
    from: u64,

    #[structopt(name = "to")]
    to: u64,
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
}

#[derive(Debug, StructOpt)]
struct CommandArgs {
    #[structopt(name = "config-file", short="c")]
    config: Option<PathBuf>,

    /// Request debug mode - no TUI
    #[structopt(name = "debug", short="d")]
    debug: bool,

    /// Request debug mode - no TUI
    #[structopt(name = "non-interactive", short="n")]
    non_interactive: bool,

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
}

#[derive(Debug, Clone, Deserialize)]
struct Pipeline {
    id: u64,
    status: String,
    r#ref: String,
    sha: String,
    web_url: String,
    created_at: DateTime<Local>,
    updated_at: DateTime<Local>,
}

#[derive(Debug, Deserialize, Clone)]
struct Job {
    id: u64,
    name: String,
    status: String,
}

use tui::{style::{Color, Style}, text::Span};

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
    UpdateMode(CommandMode),
}

struct Thread {
    gitlab: Gitlab,
    rx_cmd: mpsc::Receiver<RxCmd>,
    state: Arc<Mutex<State>>,
    rsp_sender: channel::mpsc::Sender<()>,
    current_user: User,
    mode: CommandMode,
    config: Config,
    debug: bool,
}

impl Thread {
    fn run_wrap(&mut self) -> Result<(), Error> {
        loop {
            let cmd = self.rx_cmd.recv()?;

            match cmd {
                RxCmd::Refresh => {}
                RxCmd::UpdateMode(mode) => {
                    self.mode = mode;
                }
            }

            let mut updates = 0;
            let mut resolve_pipeline_trigger = vec![];

            if self.debug {
                println!("glcim: request iteration");
            }

            let mut resolve_usernames = false;
            match &self.mode {
                CommandMode::Jobs(info) => {
                    if self.state.lock().unwrap().jobs.update {
                        let endpoint =
                            projects::pipelines::PipelineJobs::builder()
                            .project(self.config.project.clone())
                            .pipeline(info.pipeline_id)
                            .scopes(vec![
                                JobScope::Pending,
                                JobScope::Running,
                                JobScope::Failed,
                                JobScope::Success,
                                JobScope::Canceled,
                            ].into_iter())
                            .build().unwrap();
                        let endpoint = gitlab::api::paged(endpoint,
                            gitlab::api::Pagination::Limit(200));
                        let jobs : Vec<Job> = endpoint.query(&self.gitlab).unwrap();
                        let mut state = self.state.lock().unwrap();

                        if self.debug {
                            println!("glcim: jobs: {:?}", jobs);
                        }

                        if state.jobs.update {
                            state.jobs.data = Some((Instant::now(), jobs));
                            state.request_count += 1;
                            updates += 1;
                            state.jobs.updated();
                        }
                    }
                }
                CommandMode::PipeDiff(pipediff) => {
                    if self.state.lock().unwrap().pipediff.update {
                        let get_jobs = &|num| {
                            let endpoint =
                                projects::pipelines::PipelineJobs::builder()
                                .project(self.config.project.clone())
                                .pipeline(num)
                                .scopes(vec![
                                    JobScope::Pending,
                                    JobScope::Running,
                                    JobScope::Failed,
                                    JobScope::Success,
                                    JobScope::Canceled,
                                ].into_iter())
                                .build().unwrap();
                            let endpoint = gitlab::api::paged(endpoint,
                                gitlab::api::Pagination::Limit(300));
                            let jobs : Vec<Job> = endpoint.query(&self.gitlab).unwrap();
                            let mut map = std::collections::BTreeMap::new();
                            for job in jobs.into_iter() {
                                map.insert(job.name.clone(), job);
                            }
                            map
                        };

                        let from = get_jobs(pipediff.from);
                        let to = get_jobs(pipediff.to);
                        let v : Vec<itertools::EitherOrBoth<(String, Job), (String, Job)>> =
                            itertools::merge_join_by(from, to, |(k1, _), (k2, _)|
                                Ord::cmp(k1, k2)
                            ).collect();

                        let mut state = self.state.lock().unwrap();
                        if state.pipediff.update {
                            state.pipediff.data = Some((Instant::now(), v));
                            state.request_count += 1;
                            updates += 1;
                            state.pipediff.updated();
                        }
                    }
                },
                CommandMode::Pipelines(info) => {
                    resolve_usernames = info.resolve_usernames;
                    if self.state.lock().unwrap().pipelines.update {
                        let mut endpoint = projects::pipelines::Pipelines::builder();
                        endpoint.project(self.config.project.clone());
                        endpoint.order_by(PipelineOrderBy::Id);
                        if !info.all_users {
                            endpoint.username(&self.current_user.username);
                        }
                        if let Some(r#ref) = &info.r#ref {
                            endpoint.ref_(r#ref.to_owned());
                        }
                        let endpoint = endpoint.build().unwrap();
                        let endpoint = gitlab::api::paged(endpoint,
                            gitlab::api::Pagination::Limit(info.nr_pipelines));

                        let pipelines : Vec<Pipeline> = endpoint.query(&self.gitlab).unwrap();

                        if self.debug {
                            println!("glcim: pipelines: {:?}", pipelines);
                        }

                        let mut state = self.state.lock().unwrap();

                        for pipeline in pipelines.iter() {
                            if state.pipeline_trigger.get(&pipeline.id).is_none() {
                                resolve_pipeline_trigger.push(pipeline.id);
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

            if resolve_usernames {
                for id in resolve_pipeline_trigger.drain(..) {
                    let endpoint = projects::pipelines::Pipeline::builder()
                        .project(self.config.project.clone())
                        .pipeline(id)
                        .build().unwrap();
                    let pipeline : PipelineDetails = endpoint.query(&self.gitlab).unwrap();
                    let mut state = self.state.lock().unwrap();
                    state.pipeline_trigger.insert(id, pipeline.user.username);
                    state.request_count += 1;
                    updates += 1;
                    drop(state);

                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            }

            if updates > 0 {
                let _ = self.rsp_sender.try_send(());
            }
        }
    }

    fn run(&mut self) {
        let _ = self.run_wrap();
    }
}

struct Main {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    state: Arc<Mutex<State>>,
    tx_cmd: mpsc::Sender<RxCmd>,
    rsp_recv: Option<channel::mpsc::Receiver<()>>,
    prev_mode: Option<CommandMode>,
    mode: CommandMode,
    selected_pipeline: tui::widgets::TableState,
    selected_job: tui::widgets::TableState,
    selected_pipediff: tui::widgets::TableState,
    ignored_pipeline_ids: std::collections::HashSet<u64>,
    pipelines: Vec<Pipeline>,
    jobs: Vec<Job>,
    pipediffs: Vec<JobDiff>,
    config: Config,
    debug: bool,
    leave: bool,
    pipediff_hide_unchanged: bool,
    non_interactive: bool,
}

impl Main {
    fn new(opt: &CommandArgs) -> Result<Self, Error> {
        let config_path = if let Some(config) = &opt.config {
            config.clone()
        } else {
            if let Some(dir) = dirs::config_dir() {
                dir.join("glcim").join("config.toml")
            } else {
                return Err(Error::ConfigFile);
            }
        };

        let mut settings = config::Config::default();
        settings
            .merge(config::File::new(config_path.to_str().unwrap(), config::FileFormat::Toml))?
            // Add in settings from the environment (with a prefix of APP)
            // Eg.. `APP_DEBUG=1 ./target/app` would set the `debug` key
            .merge(config::Environment::with_prefix("GITLAB_CI_CONSOLE")).unwrap();

        // Print out our settings (as a HashMap)
        let config = settings.try_into::<Config>()?;
        let config2 = config.clone();

        // Create the client.
        let client = Gitlab::new(&config.hostname, &config.api_key).unwrap();

        let endpoint = users::CurrentUser::builder()
            .build().unwrap();
        let current_user: User = endpoint.query(&client).unwrap();

        let state = Arc::new(Mutex::new(State::default()));
        let state2 = state.clone();
        let (tx, rx) = mpsc::channel();
        let (rsp_sender, rsp_recv) = channel::mpsc::channel(4);

        let command = opt.command.clone();

        let debug = opt.debug;

        std::thread::spawn(move || {
            Thread {
                config: config2,
                gitlab: client,
                state: state2,
                rx_cmd: rx,
                rsp_sender,
                current_user,
                debug,
                mode: command
            }.run();
        });

        if !opt.debug && !opt.non_interactive {
            enable_raw_mode()?;
        }
        let mut stdout = stdout();
        if !opt.debug && !opt.non_interactive {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

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
            prev_mode: None,
            mode: opt.command.clone(),
            rsp_recv: Some(rsp_recv),
        })
    }

    async fn run(&mut self) -> Result<(), Error> {
        let mut reader = EventStream::new();
        let mut rsp_recv = self.rsp_recv.take().unwrap();

        while !self.leave {
            let updates : usize = {
                let mut state = self.state.lock().unwrap();
                let mut updates = 0;
                match self.mode {
                    CommandMode::Pipelines{..} => {
                        if state.pipelines.check_expiry(std::time::Duration::from_millis(10000)) {
                            updates += 1;
                        }
                    },
                    CommandMode::Jobs{..} => {
                        if state.jobs.check_expiry(std::time::Duration::from_millis(10000)) {
                            updates += 1;
                        }
                    },
                    CommandMode::PipeDiff{..} => {
                        if state.pipediff.check_expiry(std::time::Duration::from_millis(60000)) {
                            updates += 1;
                        }
                    },
                }
                updates
            };
            if updates > 0 {
                let _ = self.tx_cmd.send(RxCmd::Refresh);
            }

            if !self.debug {
                if !self.non_interactive {
                    self.draw()?;
                } else {
                    if self.check_report_non_interactive()? {
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
                        Some(Ok(event)) => { self.on_event(event)? }
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

    fn draw(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::PipeDiff{..} => self.draw_pipediff(),
            CommandMode::Pipelines{..} => self.draw_pipelines(),
            CommandMode::Jobs{..} => self.draw_jobs(),
        }
    }

    fn check_report_non_interactive(&mut self) -> Result<bool, Error> {
        match &self.mode {
            CommandMode::PipeDiff{..} => {
                return self.non_interactive_pipediff();
            }
            _ => {}
        };

        Ok(false)
    }

    fn non_interactive_pipediff(&mut self) -> Result<bool, Error> {
        let state = self.state.lock().unwrap();

        if let Some((_, pipediff)) = &state.pipediff.data {
            for res in pipediff.iter() {
                match res {
                    itertools::EitherOrBoth::Both((k, a), (_, b)) => {
                        if a.status != b.status {
                            println!("{:width$}: {} -> {}", k, a.status, b.status, width=40);
                        }
                    }
                    _ => {}
                }
            }
            return Ok(true);
        }

        Ok(false)
    }

    fn draw_pipelines(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        state.pipelines.fix_selected(&mut self.selected_pipeline,
            Some(self.pipelines.len()));
        let selected_pipeline = &mut self.selected_pipeline;
        let pipeline_ids = &mut self.pipelines;
        let ignored_pipeline_ids = &self.ignored_pipeline_ids;

        self.terminal.draw(|rect| {
            use tui::style::Modifier;
            use tui::widgets::{Table, Cell, Row, BorderType};

            let mut items: Vec<_> = vec![];
            pipeline_ids.clear();
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

                    items.push(Row::new(vec![
                        Cell::from(Span::raw(pipeline.id.to_string())),
                        Cell::from(Span::raw(user)),
                        Cell::from(status),
                        Cell::from(Span::raw(pipeline.r#ref.to_string())),
                        Cell::from(Span::raw(pipeline.sha.to_string())),
                        Cell::from(Span::raw(pipeline.created_at.to_string())),
                    ]));
                }
            }

            let pipelines = Table::new(items)
            .header(Row::new(vec![
                Cell::from(Span::styled(
                    "ID",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    "User",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
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
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .style(Style::default().fg(Color::White))
                    .title("Pipelines")
                    .border_type(BorderType::Plain),
            )
            .highlight_style(
                Style::default().bg(Color::Rgb(60, 60, 80))
            )
            .widths(&[
                Constraint::Length(8),
                Constraint::Length(15),
                Constraint::Length(10),
                Constraint::Length(30),
                Constraint::Length(12),
                Constraint::Length(19),
            ]);

            rect.render_stateful_widget(pipelines, rect.size(), selected_pipeline);
        })?;

        Ok(())
    }

    fn draw_jobs(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        state.jobs.fix_selected(&mut self.selected_job, None);
        let printed_jobs = &mut self.jobs;
        let selected_job = &mut self.selected_job;

        printed_jobs.clear();

        self.terminal.draw(|rect| {
            use tui::style::Modifier;
            use tui::widgets::{Table, Cell, Row, BorderType};

            let mut items: Vec<_> = vec![];
            if let Some((_, jobs)) = &state.jobs.data {
                for job in jobs.iter() {
                    printed_jobs.push((*job).clone());
                    let status = job.styled_status();
                    items.push(Row::new(vec![
                        Cell::from(Span::raw(job.id.to_string())),
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
            .highlight_style(
                Style::default().bg(Color::Rgb(60, 60, 80))
            )
            .widths(&[
                Constraint::Length(10),
                Constraint::Length(15),
                Constraint::Length(40),
            ]);

            rect.render_stateful_widget(jobs, rect.size(), selected_job);
        })?;

        Ok(())
    }

    fn draw_pipediff(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        state.pipediff.fix_selected(&mut self.selected_job, None);
        let printed_pipediffs = &mut self.pipediffs;
        let selected_pipediff = &mut self.selected_pipediff;

        printed_pipediffs.clear();

        let pipediff_hide_unchanged = self.pipediff_hide_unchanged;
        self.terminal.draw(|rect| {
            use tui::style::Modifier;
            use tui::widgets::{Table, Cell, Row, BorderType};

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
                    .title("Jobs")
                    .border_type(BorderType::Plain),
            )
            .highlight_style(
                Style::default().bg(Color::Rgb(60, 60, 80))
            )
            .widths(&[
                Constraint::Length(40),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Length(10),
            ]);

            rect.render_stateful_widget(pipediffs, rect.size(), selected_pipediff);
        })?;

        Ok(())
    }
    fn on_up(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        match self.mode {
            CommandMode::Jobs(_) => state.jobs.up(&mut self.selected_job, None),
            CommandMode::Pipelines(_) => state.pipelines.up(&mut self.selected_pipeline, None),
            CommandMode::PipeDiff{..} => state.pipediff.up(&mut self.selected_pipediff, None),
        }

        Ok(())
    }

    fn on_down(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        match self.mode {
            CommandMode::Jobs(_) => state.jobs.down(&mut self.selected_job, Some(self.jobs.len())),
            CommandMode::Pipelines(_) => state.pipelines.down(&mut self.selected_pipeline, Some(self.pipelines.len())),
            CommandMode::PipeDiff{..} => state.pipediff.down(&mut self.selected_pipediff, Some(self.pipediffs.len())),
        }

        Ok(())
    }

    fn on_home(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        match self.mode {
            CommandMode::Jobs(_) => state.jobs.home(&mut self.selected_job, None),
            CommandMode::Pipelines(_) => state.pipelines.home(&mut self.selected_pipeline, None),
            CommandMode::PipeDiff{..} => state.pipediff.home(&mut self.selected_pipediff, None),
        }

        Ok(())
    }

    fn on_end(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        match self.mode {
            CommandMode::Jobs(_) => state.jobs.end(&mut self.selected_job, Some(self.jobs.len())),
            CommandMode::Pipelines(_) => state.pipelines.end(&mut self.selected_pipeline, Some(self.pipelines.len())),
            CommandMode::PipeDiff{..} => state.pipediff.end(&mut self.selected_pipediff, Some(self.pipediffs.len())),
        }

        Ok(())
    }

    fn on_backspace(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(_) => {
                if let Some(mode) = self.prev_mode.take() {
                    self.mode = mode;
                    let _ = self.tx_cmd.send(RxCmd::UpdateMode(self.mode.clone()));
                }
            }
            CommandMode::PipeDiff{..} => { },
            CommandMode::Pipelines(_) => { },
        }

        Ok(())
    }

    fn on_enter_job(&self, job: &Job, pipeline_id: u64) -> Result<(), Error> {
        if let Some(open_job_command) = &self.config.hooks.open_job_command {
            let shell = std::env::var("SHELL")?;
            let mut command =
                std::process::Command::new(shell);
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

    fn on_enter(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(ref info) => {
                if let Some(selected) = self.selected_job.selected() {
                    if selected < self.jobs.len() {
                        let job = &self.jobs[selected];
                        self.on_enter_job(job, info.pipeline_id)?;
                    }
                }
            }
            CommandMode::PipeDiff(ref info) => {
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
            },
            CommandMode::Pipelines(_) => {
                if let Some(selected) = self.selected_pipeline.selected() {
                    if selected < self.pipelines.len() {
                        let pipeline = &self.pipelines[selected];
                        self.selected_job.select(None);
                        self.prev_mode = Some(std::mem::replace(&mut self.mode,
                            CommandMode::Jobs(JobsMode {
                                pipeline_id: pipeline.id
                            })
                        ));
                        let mut state = self.state.lock().unwrap();
                        state.jobs.data = None;
                        let _ = self.tx_cmd.send(RxCmd::UpdateMode(self.mode.clone()));
                    }
                }
            }
        }

        Ok(())
    }

    fn on_job_open_in_browser(&self, job_id: u64) -> Result<(), Error> {
        let url = format!("https://{}/{}/-/jobs/{}", &self.config.hostname, &self.config.project, job_id);
        let _ = webbrowser::open(&url);
        Ok(())
    }

    fn on_pipeline_open_in_browser(&self, pipeline_id: u64) -> Result<(), Error> {
        let url = format!("https://{}/{}/pipelines/{}", &self.config.hostname, &self.config.project, pipeline_id);
        let _ = webbrowser::open(&url);
        Ok(())
    }

    fn on_o_key(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(_) => {
                if let Some(selected) = self.selected_job.selected() {
                    if selected < self.jobs.len() {
                        let job = &self.jobs[selected];
                        self.on_job_open_in_browser(job.id)?;
                    }
                }
            }
            CommandMode::PipeDiff(_) => {
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
            },
            CommandMode::Pipelines(_) => {
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

    fn on_p_key(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(_) => {
            }
            CommandMode::PipeDiff(ref info) => {
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
            },
            CommandMode::Pipelines(_) => {
            }
        }

        Ok(())
    }

    fn on_delete(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(_) => {
            }
            CommandMode::PipeDiff{..} => { },
            CommandMode::Pipelines(_) => {
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

    fn on_l_key(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(_) => {
            }
            CommandMode::PipeDiff{..} => { },
            CommandMode::Pipelines(_) => {
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
                            self.terminal.clear()?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn on_event(&mut self, event: CEvent) -> Result<(), Error> {
        match event {
            CEvent::Key(key_event) => {
                match key_event.code {
                    crossterm::event::KeyCode::Char('q') => {
                        self.leave = true;
                    },
                    crossterm::event::KeyCode::Char('r') => {
                        self.ignored_pipeline_ids.clear();
                    },
                    crossterm::event::KeyCode::Char('h') => {
                        self.pipediff_hide_unchanged = !self.pipediff_hide_unchanged;
                    },
                    crossterm::event::KeyCode::Char('l') => {
                        self.on_l_key()?;
                    },
                    crossterm::event::KeyCode::Char('p') => {
                        self.on_p_key()?;
                    },
                    crossterm::event::KeyCode::Enter => {
                        self.on_enter()?;
                    },
                    crossterm::event::KeyCode::Char('o') => {
                        self.on_o_key()?;
                    },
                    crossterm::event::KeyCode::Backspace => {
                        self.on_backspace()?;
                    },
                    crossterm::event::KeyCode::Delete => {
                        self.on_delete()?;
                    },
                    crossterm::event::KeyCode::Up => {
                        self.on_up()?;
                    },
                    crossterm::event::KeyCode::Down => {
                        self.on_down()?;
                    },
                    crossterm::event::KeyCode::PageUp => {
                        for _ in 0..crossterm::terminal::size()?.1.saturating_sub(2) {
                            self.on_up()?;
                        }
                    },
                    crossterm::event::KeyCode::PageDown => {
                        for _ in 0..crossterm::terminal::size()?.1.saturating_sub(2) {
                            self.on_down()?;
                        }
                    },
                    crossterm::event::KeyCode::Home => {
                        self.on_home()?;
                    },
                    crossterm::event::KeyCode::End => {
                        self.on_end()?;
                    },
                    _ => { }
                }
            }
            _ => {},
        }

        Ok(())
    }

    fn gain_terminal(&mut self) -> Result<(), Error> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        Ok(())
    }

    fn release_terminal(&mut self) -> Result<(), Error> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

fn main_wrap() -> Result<(), Error> {
    let opt = CommandArgs::from_args();
    let mut glcim = Main::new(&opt)?;

    let (mut glcim, v) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(3)
        .build()
        .unwrap()
        .block_on(async {
            let v = glcim.run().await;
            (glcim, v)
        });

    if !opt.debug && !opt.non_interactive {
        glcim.release_terminal()?;
    }

    v?;
    Ok(())
}

fn main() {
    match main_wrap() {
        Ok(()) => {},
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(-1);
        }
    }
}

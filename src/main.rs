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
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
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
    #[structopt(name = "all", short="a")]
    all: bool,
}

#[derive(Debug, StructOpt, Clone)]
struct JobsMode {
    #[structopt(name = "pipeline-id", short="p")]
    pipeline_id: u64,
}

#[derive(Debug, StructOpt, Clone)]
enum CommandMode {
    /// Show all pipelines for a project, or specific to current user
    #[structopt(name = "pipelines")]
    Pipelines(PipelinesMode),

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

    #[structopt(subcommand)]
    command: CommandMode,
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    api_key: String,
    hostname: String,
    project: String,

    #[serde(default)]
    hooks: ConfigHooks,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct ConfigHooks {
    /// Shell command to execute when opening a Job (using $SHELL).
    ///
    /// The following environment variables will be defined:
    ///
    /// $GLCIM_JOB_ID, $GLCIM_PIPELINE_ID, $GLCIM_HOSTNAME, $GLCIM_API_KEY,
    /// $GLCIM_PROJECT
    open_job_command: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Pipeline {
    id: u64,
    status: String,
    r#ref: String,
    sha: String,
    web_url: String,
    created_at: DateTime<Local>,
    updated_at: DateTime<Local>,
}

#[derive(Debug, Deserialize)]
struct Job {
    id: u64,
    name: String,
    status: String,
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

}

#[derive(Default)]
struct State {
    request_count: u64,
    pipelines: Updatable<Vec<Pipeline>>,
    jobs: Updatable<Vec<Job>>,
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

            match &self.mode {
                CommandMode::Jobs(info) => {
                    if self.state.lock().unwrap().jobs.update {
                        let endpoint =
                            projects::pipelines::PipelineJobs::builder()
                            .project(self.config.project.clone())
                            .pipeline(info.pipeline_id)
                            .scopes(vec![
                                JobScope::Created,
                                JobScope::Pending,
                                JobScope::Running,
                                JobScope::Failed,
                                JobScope::Success,
                                JobScope::Canceled,
                                JobScope::Skipped,
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
                CommandMode::Pipelines(info) => {
                    if self.state.lock().unwrap().pipelines.update {
                        let mut endpoint = projects::pipelines::Pipelines::builder();
                        endpoint.project(self.config.project.clone());
                        endpoint.order_by(PipelineOrderBy::Id);
                        if !info.all {
                            endpoint.username(&self.current_user.username);
                        }
                        let endpoint = endpoint.build().unwrap();
                        let endpoint = gitlab::api::paged(endpoint,
                            gitlab::api::Pagination::Limit(30));

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

            for id in resolve_pipeline_trigger.drain(..) {
                let endpoint = projects::pipelines::Pipeline::builder()
                    .project(self.config.project.clone())
                    .pipeline(id)
                    .build().unwrap();
                let pipeline : PipelineDetails = endpoint.query(&self.gitlab).unwrap();
                if self.debug {
                    println!("glcim: pipeline: {:?}", pipeline);
                }
                let mut state = self.state.lock().unwrap();
                state.pipeline_trigger.insert(id, pipeline.user.username);
                state.request_count += 1;
                updates += 1;
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
    ignored_pipeline_ids: std::collections::HashSet<u64>,
    pipeline_ids: Vec<u64>,
    job_ids: Vec<u64>,
    config: Config,
    debug: bool,
    leave: bool,
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

        if !opt.debug {
            enable_raw_mode()?;
        }
        let mut stdout = stdout();
        if !opt.debug {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        Ok(Self {
            terminal,
            state,
            selected_pipeline: Default::default(),
            selected_job: Default::default(),
            ignored_pipeline_ids: Default::default(),
            pipeline_ids: vec![],
            job_ids: vec![],
            leave: false,
            tx_cmd: tx,
            config,
            debug: opt.debug,
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
                }
                updates
            };
            if updates > 0 {
                let _ = self.tx_cmd.send(RxCmd::Refresh);
            }

            if !self.debug {
                self.draw()?;
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
            CommandMode::Pipelines{..} => self.draw_pipelines(),
            CommandMode::Jobs{..} => self.draw_jobs(),
        }
    }

    fn draw_pipelines(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        state.pipelines.fix_selected(&mut self.selected_pipeline,
            Some(self.pipeline_ids.len()));
        let selected_pipeline = &mut self.selected_pipeline;
        let pipeline_ids = &mut self.pipeline_ids;
        let ignored_pipeline_ids = &self.ignored_pipeline_ids;

        self.terminal.draw(|rect| {
            use tui::{style::{Color, Modifier, Style}, text::Span};
            use tui::widgets::{Table, Cell, Row, BorderType};

            let mut items: Vec<_> = vec![];
            pipeline_ids.clear();
            if let Some((_, pipelines)) = &state.pipelines.data {
                for pipeline in pipelines.iter() {
                    if ignored_pipeline_ids.get(&pipeline.id).is_some() {
                        continue;
                    }

                    pipeline_ids.push(pipeline.id);
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
        let job_ids = &mut self.job_ids;
        let selected_job = &mut self.selected_job;

        job_ids.clear();

        self.terminal.draw(|rect| {
            use tui::{style::{Color, Modifier, Style}, text::Span};
            use tui::widgets::{Table, Cell, Row, BorderType};

            let mut items: Vec<_> = vec![];
            if let Some((_, jobs)) = &state.jobs.data {
                for job in jobs.iter() {
                    job_ids.push(job.id);
                    let status = Span::styled(
                        &job.status,
                        Style::default().fg(match job.status.as_str() {
                            "failed" => Color::Red,
                            "running" => Color::Cyan,
                            "success" => Color::Green,
                            "canceled" => Color::Gray,
                            _ => Color::White,
                        }),
                    );

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

    fn on_up(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        match self.mode {
            CommandMode::Jobs(_) => {
                if let Some(selected) = self.selected_job.selected() {
                    if selected > 0 {
                        self.selected_job.select(Some(selected - 1));
                    }
                } else {
                    self.selected_job.select(Some(0));
                }

                state.jobs.fix_selected(&mut self.selected_job, None)
            }
            CommandMode::Pipelines(_) => {
                if let Some(selected) = self.selected_pipeline.selected() {
                    if selected > 0 {
                        self.selected_pipeline.select(Some(selected - 1));
                    }
                } else {
                    self.selected_pipeline.select(Some(0));
                }

                state.pipelines.fix_selected(&mut self.selected_pipeline,
                    Some(self.pipeline_ids.len()));
            }
        }

        Ok(())
    }

    fn on_down(&mut self) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        match self.mode {
            CommandMode::Jobs(_) => {
                if let Some(selected) = self.selected_job.selected() {
                    self.selected_job.select(Some(selected + 1));
                } else {
                    self.selected_job.select(Some(0));
                }

                state.jobs.fix_selected(&mut self.selected_job, None)
            }
            CommandMode::Pipelines(_) => {
                if let Some(selected) = self.selected_pipeline.selected() {
                    self.selected_pipeline.select(Some(selected + 1));
                } else {
                    self.selected_pipeline.select(Some(0));
                }

                state.pipelines.fix_selected(&mut self.selected_pipeline,
                    Some(self.pipeline_ids.len()));
            }
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
            CommandMode::Pipelines(_) => {
            }
        }

        Ok(())
    }

    fn on_enter(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(ref info) => {
                if let Some(open_job_command) = &self.config.hooks.open_job_command {
                    if let Some(selected) = self.selected_job.selected() {
                        if selected < self.job_ids.len() {
                            let id = self.job_ids[selected];
                            let shell = std::env::var("SHELL")?;
                            let mut command =
                                std::process::Command::new(shell);
                            command.arg("-c");
                            command.arg(open_job_command);
                            command.env("GLCIM_JOB_ID", format!("{}", id));
                            command.env("GLCIM_PIPELINE_ID", format!("{}",
                                info.pipeline_id));
                            command.env("GLCIM_PROJECT", &self.config.project);
                            command.env("GLCIM_HOSTNAME", &self.config.hostname);
                            command.env("GLCIM_API_KEY", &self.config.api_key);
                            if let Ok(mut v) = command.spawn() {
                                let _ = v.wait();
                            }
                            // TODO: report error
                        }
                    }
                }
            }
            CommandMode::Pipelines(_) => {
                if let Some(selected) = self.selected_pipeline.selected() {
                    if selected < self.pipeline_ids.len() {
                        let id = self.pipeline_ids[selected];
                        self.selected_job.select(None);
                        self.prev_mode = Some(std::mem::replace(&mut self.mode,
                            CommandMode::Jobs(JobsMode {
                                pipeline_id: id
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

    fn on_delete(&mut self) -> Result<(), Error> {
        match self.mode {
            CommandMode::Jobs(_) => {
            }
            CommandMode::Pipelines(_) => {
                if let Some(selected) = self.selected_pipeline.selected() {
                    if selected < self.pipeline_ids.len() {
                        let id = self.pipeline_ids[selected];
                        self.ignored_pipeline_ids.insert(id);
                        if selected > 0 && selected + 1 == self.pipeline_ids.len() {
                            self.selected_pipeline.select(Some(selected - 1));
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
                    crossterm::event::KeyCode::Enter => {
                        self.on_enter()?;
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
                    _ => { }
                }
            }
            _ => {},
        }

        Ok(())
    }
}

fn main_wrap() -> Result<(), Error> {
    let opt = CommandArgs::from_args();
    let mut glcim = Main::new(&opt)?;

    if !opt.debug {
    }

    let (mut glcim, v) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(3)
        .build()
        .unwrap()
        .block_on(async {
            let v = glcim.run().await;
            (glcim, v)
        });

    if !opt.debug {
        disable_raw_mode()?;
        execute!(
            glcim.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        glcim.terminal.show_cursor()?;
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

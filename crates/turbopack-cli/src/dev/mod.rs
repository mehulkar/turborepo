use std::{
    collections::HashSet,
    env::current_dir,
    future::{join, Future},
    io::{stdout, Write},
    net::{IpAddr, SocketAddr},
    path::{PathBuf, MAIN_SEPARATOR},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use turbo_tasks::{
    util::{FormatBytes, FormatDuration},
    StatsType, TransientInstance, TurboTasks, TurboTasksBackendApi, UpdateInfo, Value, Vc,
};
use turbo_tasks_fs::FileSystem;
use turbo_tasks_malloc::TurboMalloc;
use turbo_tasks_memory::MemoryBackend;
use turbopack::evaluate_context::node_build_environment;
use turbopack_browser::BrowserChunkingContext;
use turbopack_cli_utils::issue::{ConsoleUi, LogOptions};
use turbopack_core::{
    issue::{IssueReporter, IssueSeverity},
    resolve::parse::Request,
    server_fs::ServerFileSystem,
};
use turbopack_dev_server::{
    introspect::IntrospectionSource,
    source::{
        combined::CombinedContentSource, router::PrefixedRouterContentSource,
        static_assets::StaticAssetsContentSource, ContentSource,
    },
    DevServer, DevServerBuilder,
};
use turbopack_env::dotenv::load_env;
use turbopack_node::execution_context::ExecutionContext;

use self::web_entry_source::create_web_entry_source;
use crate::{
    arguments::DevArguments,
    contexts::NodeEnv,
    util::{
        normalize_dirs, normalize_entries, output_fs, project_fs, EntryRequest, NormalizedDirs,
    },
};

pub(crate) mod turbo_tasks_viz;
pub(crate) mod web_entry_source;

pub struct TurbopackDevServerBuilder {
    turbo_tasks: Arc<TurboTasks<MemoryBackend>>,
    project_dir: String,
    root_dir: String,
    entry_requests: Vec<EntryRequest>,
    eager_compile: bool,
    hostname: Option<IpAddr>,
    issue_reporter: Option<Box<dyn IssueReporterProvider>>,
    port: Option<u16>,
    browserslist_query: String,
    log_level: IssueSeverity,
    show_all: bool,
    log_detail: bool,
    allow_retry: bool,
}

impl TurbopackDevServerBuilder {
    pub fn new(
        turbo_tasks: Arc<TurboTasks<MemoryBackend>>,
        project_dir: String,
        root_dir: String,
    ) -> TurbopackDevServerBuilder {
        TurbopackDevServerBuilder {
            turbo_tasks,
            project_dir,
            root_dir,
            entry_requests: vec![],
            eager_compile: false,
            hostname: None,
            issue_reporter: None,
            port: None,
            browserslist_query: "last 1 Chrome versions, last 1 Firefox versions, last 1 Safari \
                                 versions, last 1 Edge versions"
                .to_owned(),
            log_level: IssueSeverity::Warning,
            show_all: false,
            log_detail: false,
            allow_retry: false,
        }
    }

    pub fn entry_request(mut self, entry_asset_path: EntryRequest) -> TurbopackDevServerBuilder {
        self.entry_requests.push(entry_asset_path);
        self
    }

    pub fn eager_compile(mut self, eager_compile: bool) -> TurbopackDevServerBuilder {
        self.eager_compile = eager_compile;
        self
    }

    pub fn hostname(mut self, hostname: IpAddr) -> TurbopackDevServerBuilder {
        self.hostname = Some(hostname);
        self
    }

    pub fn port(mut self, port: u16) -> TurbopackDevServerBuilder {
        self.port = Some(port);
        self
    }

    pub fn browserslist_query(mut self, browserslist_query: String) -> TurbopackDevServerBuilder {
        self.browserslist_query = browserslist_query;
        self
    }

    pub fn log_level(mut self, log_level: IssueSeverity) -> TurbopackDevServerBuilder {
        self.log_level = log_level;
        self
    }

    pub fn show_all(mut self, show_all: bool) -> TurbopackDevServerBuilder {
        self.show_all = show_all;
        self
    }

    pub fn allow_retry(mut self, allow_retry: bool) -> TurbopackDevServerBuilder {
        self.allow_retry = allow_retry;
        self
    }

    pub fn log_detail(mut self, log_detail: bool) -> TurbopackDevServerBuilder {
        self.log_detail = log_detail;
        self
    }

    pub fn issue_reporter(
        mut self,
        issue_reporter: Box<dyn IssueReporterProvider>,
    ) -> TurbopackDevServerBuilder {
        self.issue_reporter = Some(issue_reporter);
        self
    }

    /// Attempts to find an open port to bind.
    fn find_port(&self, host: IpAddr, port: u16, max_attempts: u16) -> Result<DevServerBuilder> {
        // max_attempts of 1 means we loop 0 times.
        let max_attempts = max_attempts - 1;
        let mut attempts = 0;
        loop {
            let current_port = port + attempts;
            let addr = SocketAddr::new(host, current_port);
            let listen_result = DevServer::listen(addr);

            if let Err(e) = &listen_result {
                if self.allow_retry && attempts < max_attempts {
                    // Returned error from `listen` is not `std::io::Error` but `anyhow::Error`,
                    // so we need to access its source to check if it is
                    // `std::io::ErrorKind::AddrInUse`.
                    let should_retry = e
                        .source()
                        .and_then(|e| {
                            e.downcast_ref::<std::io::Error>()
                                .map(|e| e.kind() == std::io::ErrorKind::AddrInUse)
                        })
                        .unwrap_or(false);

                    if should_retry {
                        println!(
                            "{} - Port {} is in use, trying {} instead",
                            "warn ".yellow(),
                            current_port,
                            current_port + 1
                        );
                        attempts += 1;
                        continue;
                    }
                }
            }

            return listen_result;
        }
    }

    pub async fn build(self) -> Result<DevServer> {
        let port = self.port.context("port must be set")?;
        let host = self.hostname.context("hostname must be set")?;

        let server = self.find_port(host, port, 10)?;

        let turbo_tasks = self.turbo_tasks;
        let project_dir = self.project_dir;
        let root_dir = self.root_dir;
        let eager_compile = self.eager_compile;
        let show_all = self.show_all;
        let log_detail = self.log_detail;
        let browserslist_query = self.browserslist_query;
        let log_args = Arc::new(LogOptions {
            current_dir: current_dir().unwrap(),
            project_dir: PathBuf::from(project_dir.clone()),
            show_all,
            log_detail,
            log_level: self.log_level,
        });
        let entry_requests = Arc::new(self.entry_requests);
        let tasks = turbo_tasks.clone();
        let issue_provider = self.issue_reporter.unwrap_or_else(|| {
            // Initialize a ConsoleUi reporter if no custom reporter was provided
            Box::new(move || Vc::upcast(ConsoleUi::new(log_args.clone().into())))
        });

        let source = move || {
            source(
                root_dir.clone(),
                project_dir.clone(),
                entry_requests.clone().into(),
                eager_compile,
                turbo_tasks.clone().into(),
                browserslist_query.clone(),
            )
        };

        let issue_reporter_arc = Arc::new(move || issue_provider.get_issue_reporter());
        Ok(server.serve(tasks, source, issue_reporter_arc))
    }
}

#[turbo_tasks::function]
async fn source(
    root_dir: String,
    project_dir: String,
    entry_requests: TransientInstance<Vec<EntryRequest>>,
    eager_compile: bool,
    turbo_tasks: TransientInstance<TurboTasks<MemoryBackend>>,
    browserslist_query: String,
) -> Result<Vc<Box<dyn ContentSource>>> {
    let project_relative = project_dir.strip_prefix(&root_dir).unwrap();
    let project_relative = project_relative
        .strip_prefix(MAIN_SEPARATOR)
        .unwrap_or(project_relative)
        .replace(MAIN_SEPARATOR, "/");

    let output_fs = output_fs(project_dir);
    let fs = project_fs(root_dir);
    let project_path: Vc<turbo_tasks_fs::FileSystemPath> = fs.root().join(project_relative);

    let env = load_env(project_path);
    let build_output_root = output_fs.root().join(".turbopack/build".to_string());

    let build_chunking_context = BrowserChunkingContext::builder(
        project_path,
        build_output_root,
        build_output_root,
        build_output_root.join("chunks".to_string()),
        build_output_root.join("assets".to_string()),
        node_build_environment(),
    )
    .build();

    let execution_context =
        ExecutionContext::new(project_path, Vc::upcast(build_chunking_context), env);

    let server_fs = Vc::upcast::<Box<dyn FileSystem>>(ServerFileSystem::new());
    let server_root = server_fs.root();
    let entry_requests = entry_requests
        .iter()
        .map(|r| match r {
            EntryRequest::Relative(p) => {
                Request::relative(Value::new(p.clone().into()), Default::default(), false)
            }
            EntryRequest::Module(m, p) => {
                Request::module(m.clone(), Value::new(p.clone().into()), Default::default())
            }
        })
        .collect();

    let web_source = create_web_entry_source(
        project_path,
        execution_context,
        entry_requests,
        server_root,
        env,
        eager_compile,
        NodeEnv::Development.cell(),
        browserslist_query,
    );
    let viz = Vc::upcast(turbo_tasks_viz::TurboTasksSource::new(turbo_tasks.into()));
    let static_source = Vc::upcast(StaticAssetsContentSource::new(
        String::new(),
        project_path.join("public".to_string()),
    ));
    let main_source = CombinedContentSource::new(vec![static_source, web_source]);
    let introspect = Vc::upcast(
        IntrospectionSource {
            roots: HashSet::from([Vc::upcast(main_source)]),
        }
        .cell(),
    );
    let main_source = Vc::upcast(main_source);
    let source = Vc::upcast(PrefixedRouterContentSource::new(
        Default::default(),
        vec![
            ("__turbopack__".to_string(), introspect),
            ("__turbo_tasks__".to_string(), viz),
        ],
        main_source,
    ));

    Ok(source)
}

pub fn register() {
    turbopack::register();
    include!(concat!(env!("OUT_DIR"), "/register.rs"));
}

/// Start a devserver with the given args.
pub async fn start_server(args: &DevArguments) -> Result<()> {
    let start = Instant::now();

    #[cfg(feature = "tokio_console")]
    console_subscriber::init();
    register();

    let NormalizedDirs {
        project_dir,
        root_dir,
    } = normalize_dirs(&args.common.dir, &args.common.root)?;

    let tt = TurboTasks::new(MemoryBackend::new(
        args.common
            .memory_limit
            .map_or(usize::MAX, |l| l * 1024 * 1024),
    ));

    let stats_type = match args.common.full_stats {
        true => StatsType::Full,
        false => StatsType::Essential,
    };
    tt.set_stats_type(stats_type);

    let tt_clone = tt.clone();

    let mut server = TurbopackDevServerBuilder::new(tt, project_dir, root_dir)
        .eager_compile(args.eager_compile)
        .hostname(args.hostname)
        .port(args.port)
        .log_detail(args.common.log_detail)
        .show_all(args.common.show_all)
        .log_level(
            args.common
                .log_level
                .map_or_else(|| IssueSeverity::Warning, |l| l.0),
        );

    for entry in normalize_entries(&args.common.entries) {
        server = server.entry_request(EntryRequest::Relative(entry))
    }

    #[cfg(feature = "serializable")]
    {
        server = server.allow_retry(args.allow_retry);
    }

    let server = server.build().await?;

    {
        let addr = &server.addr;
        let hostname = if addr.ip().is_loopback() || addr.ip().is_unspecified() {
            "localhost".to_string()
        } else if addr.is_ipv6() {
            // When using an IPv6 address, we need to surround the IP in brackets to
            // distinguish it from the port's `:`.
            format!("[{}]", addr.ip())
        } else {
            addr.ip().to_string()
        };
        let index_uri = match addr.port() {
            443 => format!("https://{hostname}"),
            80 => format!("http://{hostname}"),
            port => format!("http://{hostname}:{port}"),
        };
        println!(
            "{} - started server on {}, url: {}",
            "ready".green(),
            server.addr,
            index_uri
        );
        if !args.no_open {
            let _ = webbrowser::open(&index_uri);
        }
    }

    let stats_future = async move {
        if args.common.log_detail {
            println!(
                "{event_type} - initial compilation {start} ({memory})",
                event_type = "event".purple(),
                start = FormatDuration(start.elapsed()),
                memory = FormatBytes(TurboMalloc::memory_usage())
            );
        }

        let mut progress_counter = 0;
        loop {
            let update_future = profile_timeout(
                tt_clone.as_ref(),
                tt_clone.aggregated_update_info(Duration::from_millis(100), Duration::MAX),
            );

            if let Some(UpdateInfo {
                duration,
                tasks,
                reasons,
                ..
            }) = update_future.await
            {
                progress_counter = 0;
                match (args.common.log_detail, !reasons.is_empty()) {
                    (true, true) => {
                        println!(
                            "\x1b[2K{event_type} - {reasons} {duration} ({tasks} tasks, {memory})",
                            event_type = "event".purple(),
                            duration = FormatDuration(duration),
                            tasks = tasks,
                            memory = FormatBytes(TurboMalloc::memory_usage())
                        );
                    }
                    (true, false) => {
                        println!(
                            "\x1b[2K{event_type} - compilation {duration} ({tasks} tasks, \
                             {memory})",
                            event_type = "event".purple(),
                            duration = FormatDuration(duration),
                            tasks = tasks,
                            memory = FormatBytes(TurboMalloc::memory_usage())
                        );
                    }
                    (false, true) => {
                        println!(
                            "\x1b[2K{event_type} - {reasons} {duration}",
                            event_type = "event".purple(),
                            duration = FormatDuration(duration),
                        );
                    }
                    (false, false) => {
                        if duration > Duration::from_secs(1) {
                            println!(
                                "\x1b[2K{event_type} - compilation {duration}",
                                event_type = "event".purple(),
                                duration = FormatDuration(duration),
                            );
                        }
                    }
                }
            } else {
                progress_counter += 1;
                if args.common.log_detail {
                    print!(
                        "\x1b[2K{event_type} - updating for {progress_counter}s... ({memory})\r",
                        event_type = "event".purple(),
                        memory = FormatBytes(TurboMalloc::memory_usage())
                    );
                } else {
                    print!(
                        "\x1b[2K{event_type} - updating for {progress_counter}s...\r",
                        event_type = "event".purple(),
                    );
                }
                let _ = stdout().lock().flush();
            }
        }
    };

    join!(stats_future, async { server.future.await.unwrap() }).await;

    Ok(())
}

#[cfg(feature = "profile")]
// When profiling, exits the process when no new updates have been received for
// a given timeout and there are no more tasks in progress.
async fn profile_timeout<T>(tt: &TurboTasks<MemoryBackend>, future: impl Future<Output = T>) -> T {
    /// How long to wait in between updates before force-exiting the process
    /// during profiling.
    const PROFILE_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

    futures::pin_mut!(future);
    loop {
        match tokio::time::timeout(PROFILE_EXIT_TIMEOUT, &mut future).await {
            Ok(res) => return res,
            Err(_) => {
                if tt.get_in_progress_count() == 0 {
                    std::process::exit(0)
                }
            }
        }
    }
}

#[cfg(not(feature = "profile"))]
fn profile_timeout<T>(
    _tt: &TurboTasks<MemoryBackend>,
    future: impl Future<Output = T>,
) -> impl Future<Output = T> {
    future
}

pub trait IssueReporterProvider: Send + Sync + 'static {
    fn get_issue_reporter(&self) -> Vc<Box<dyn IssueReporter>>;
}

impl<T> IssueReporterProvider for T
where
    T: Fn() -> Vc<Box<dyn IssueReporter>> + Send + Sync + Clone + 'static,
{
    fn get_issue_reporter(&self) -> Vc<Box<dyn IssueReporter>> {
        self()
    }
}

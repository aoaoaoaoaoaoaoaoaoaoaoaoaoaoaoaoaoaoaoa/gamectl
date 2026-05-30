use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fmt::Write as FmtWrite,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    marker::PhantomData,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre::{Context, Result, bail, eyre};
use md5::{Digest, Md5};
use reqwest::{Client as HttpClient, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

mod itch;

use itch::{ItchAction, run_itch};

const CONFIG_ENV: &str = "GAMECTL_CONFIG";
const DEFAULT_MANIFEST_RELATIVE: &str = "manifests/games.toml";
const DEFAULT_ROOT: &str = "~/media/games";
const MANIFEST_ENV: &str = "GAMECTL_MANIFEST";
const ROOT_ENV: &str = "GAMECTL_ROOT";
const GOG_CLIENT_ID: &str = "46899977096215655";
const GOG_CLIENT_SECRET: &str = "9d85c43b1482497dbbce61f6e4aa173a433796eeae2ca8c5f6129f2dc4de46d9";
const GOG_REDIRECT_URI: &str = "https://embed.gog.com/on_login_success?origin=client";
pub(crate) const USER_AGENT: &str = concat!("gamectl/", env!("CARGO_PKG_VERSION"));

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    Cli::parse().run().await
}

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[arg(long, global = true, value_name = "PATH")]
    manifest: Option<PathBuf>,

    #[arg(long, global = true, value_name = "PATH")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Action,
}

impl Cli {
    async fn run(self) -> Result<()> {
        let paths = RuntimePaths::resolve(&self)?;

        match self.command {
            Action::Init { force } => init_manifest(&paths.manifest_path, &paths.root, force),
            Action::List { json } => {
                let manifest = Manifest::load(&paths.manifest_path, &paths.home, &paths.root)?;
                list_games(&manifest, json)
            }
            Action::Show { game, json } => {
                let manifest = Manifest::load(&paths.manifest_path, &paths.home, &paths.root)?;
                let resolved = manifest.resolve_game(&game)?;
                show_game(&resolved, json)
            }
            Action::Store { command } => run_store(command, &paths).await,
            Action::Register(args) => {
                register_game(&paths.manifest_path, &paths.home, &paths.root, args)
            }
            Action::Play {
                game,
                dry_run,
                args,
            } => {
                let manifest = Manifest::load(&paths.manifest_path, &paths.home, &paths.root)?;
                let resolved = manifest.resolve_game(&game)?;
                let mut invocation = resolved.launch_invocation()?;
                invocation.argv.extend(args.into_iter().map(PathText::from));
                run_invocation(invocation, dry_run)
            }
            Action::Hook {
                game,
                hook,
                dry_run,
            } => {
                let manifest = Manifest::load(&paths.manifest_path, &paths.home, &paths.root)?;
                let resolved = manifest.resolve_game(&game)?;
                fs::create_dir_all(&resolved.work_dir)?;
                let invocation = resolved.hook_invocation(hook)?;
                run_invocation(invocation, dry_run)
            }
            Action::Desktop {
                game,
                output,
                print,
            } => {
                let manifest = Manifest::load(&paths.manifest_path, &paths.home, &paths.root)?;
                let resolved = manifest.resolve_game(&game)?;
                write_desktop_entry(&resolved, &paths.manifest_path, output, print, &paths.home)
            }
            Action::Paths => {
                print_paths(&paths);
                Ok(())
            }
        }
    }

    fn config_path(&self, home: &Path) -> PathBuf {
        if let Some(path) = &self.config {
            return normalize_path(path, home, Path::new("."));
        }
        if let Some(path) = env::var_os(CONFIG_ENV) {
            return normalize_path(PathBuf::from(path), home, Path::new("."));
        }
        xdg_config_home(home).join("gamectl").join("config.toml")
    }

    fn root_path(&self, config: &GamectlConfig, home: &Path) -> PathBuf {
        if let Some(path) = &self.root {
            return normalize_path(path, home, Path::new("."));
        }
        if let Some(path) = env::var_os(ROOT_ENV) {
            return normalize_path(PathBuf::from(path), home, Path::new("."));
        }
        config
            .root_path(home)
            .unwrap_or_else(|| expand_path(DEFAULT_ROOT, home))
    }

    fn manifest_path(&self, config: &GamectlConfig, home: &Path, root: &Path) -> PathBuf {
        if let Some(path) = &self.manifest {
            return normalize_path(path, home, Path::new("."));
        }
        if let Some(path) = env::var_os(MANIFEST_ENV) {
            return normalize_path(PathBuf::from(path), home, Path::new("."));
        }
        config
            .manifest_path(home, root)
            .unwrap_or_else(|| root.join(DEFAULT_MANIFEST_RELATIVE))
    }
}

#[derive(Debug)]
pub(crate) struct RuntimePaths {
    pub(crate) home: PathBuf,
    config_path: PathBuf,
    pub(crate) root: PathBuf,
    pub(crate) manifest_path: PathBuf,
}

impl RuntimePaths {
    fn resolve(cli: &Cli) -> Result<Self> {
        let home = home_dir()?;
        let config_path = cli.config_path(&home);
        let config = GamectlConfig::load(&config_path)?;
        let root = cli.root_path(&config, &home);
        let manifest_path = cli.manifest_path(&config, &home, &root);
        Ok(Self {
            home,
            config_path,
            root,
            manifest_path,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct GamectlConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    manifest: Option<PathBuf>,
}

impl GamectlConfig {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
    }

    fn root_path(&self, home: &Path) -> Option<PathBuf> {
        self.root
            .as_deref()
            .map(|path| normalize_path(path, home, Path::new(".")))
    }

    fn manifest_path(&self, home: &Path, root: &Path) -> Option<PathBuf> {
        self.manifest
            .as_deref()
            .map(|path| normalize_path(path, home, root))
    }
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Create a starter manifest and base game directories.
    Init {
        /// Overwrite an existing manifest.
        #[arg(long)]
        force: bool,
    },

    /// List known games.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show one game's resolved launch metadata.
    Show {
        game: String,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Add or replace one manifest entry.
    Register(RegisterArgs),

    /// Install, update, and authenticate through game stores.
    Store {
        #[command(subcommand)]
        command: StoreAction,
    },

    /// Play a native Linux game.
    Play {
        game: String,

        /// Print the command without executing it.
        #[arg(long)]
        dry_run: bool,

        /// Extra arguments passed to the game after `--`.
        #[arg(last = true)]
        args: Vec<OsString>,
    },

    /// Execute a configured install/update/repair hook.
    Hook {
        game: String,
        hook: Hook,

        /// Print the command without executing it.
        #[arg(long)]
        dry_run: bool,
    },

    /// Generate a freedesktop .desktop launcher.
    Desktop {
        game: String,

        /// Write to this path instead of ~/.local/share/applications.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Print the entry instead of writing it.
        #[arg(long)]
        print: bool,
    },

    /// Print default paths used by gamectl.
    Paths,
}

#[derive(Debug, Subcommand)]
enum StoreAction {
    /// Native Linux games from GOG offline installers.
    Gog {
        #[command(subcommand)]
        command: GogAction,
    },

    /// Native Linux games from itch.io.
    Itch {
        #[command(subcommand)]
        command: ItchAction,
    },
}

#[derive(Debug, Parser)]
struct RegisterArgs {
    id: String,

    #[arg(long)]
    title: Option<String>,

    #[arg(long)]
    source: Option<String>,

    #[arg(long)]
    install_dir: PathBuf,

    #[arg(long)]
    work_dir: Option<PathBuf>,

    #[arg(long)]
    launch: String,

    #[arg(long = "arg")]
    args: Vec<String>,

    #[arg(long = "env", value_parser = parse_env_pair)]
    env: Vec<(String, String)>,

    #[arg(long = "tag")]
    tags: Vec<String>,

    #[arg(long)]
    icon: Option<PathBuf>,

    #[arg(long)]
    terminal: bool,

    #[arg(long)]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum GogAction {
    /// Manage GOG authentication.
    Auth {
        #[command(subcommand)]
        command: GogAuthAction,
    },

    /// Show the selected Linux installer without downloading it.
    Info {
        product: String,

        #[arg(long, default_value = "linux")]
        os: String,

        #[arg(long, default_value = "en")]
        language: String,

        #[arg(long)]
        json: bool,
    },

    /// Download, verify, sandbox-extract, and manifest-register a GOG game.
    Install {
        product: String,

        #[arg(long, default_value = "linux")]
        os: String,

        #[arg(long, default_value = "en")]
        language: String,

        #[arg(long)]
        id: Option<String>,

        #[arg(long)]
        force: bool,

        /// Do not use systemd-run sandboxing for archive extraction.
        #[arg(long)]
        no_sandbox: bool,
    },
}

#[derive(Debug, Subcommand)]
enum GogAuthAction {
    /// Import an existing Lutris GOG token into gamectl's private XDG state.
    ImportLutris {
        #[arg(long, default_value = "~/.cache/lutris/.gog.token")]
        path: PathBuf,
    },

    /// Print the login URL, or exchange a returned authorization code.
    Login {
        #[arg(long)]
        code: Option<String>,
    },

    /// Show whether a stored token exists and when it expires.
    Status,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Hook {
    Install,
    Update,
    Repair,
}

impl Hook {
    const fn name(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Repair => "repair",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Manifest {
    #[serde(default)]
    root: Option<PathBuf>,

    #[serde(default)]
    games: BTreeMap<String, Game>,

    #[serde(skip)]
    home: PathBuf,

    #[serde(skip)]
    root_resolved: PathBuf,
}

impl Manifest {
    fn empty(root: PathBuf, home: &Path) -> Self {
        let root_resolved = normalize_path(&root, home, Path::new("."));
        Self {
            root: Some(root),
            games: BTreeMap::new(),
            home: home.to_path_buf(),
            root_resolved,
        }
    }

    fn load(path: &Path, home: &Path, fallback_root: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut manifest: Self =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        manifest.home = home.to_path_buf();
        manifest.root_resolved = manifest.root.as_deref().map_or_else(
            || fallback_root.to_path_buf(),
            |path| normalize_path(path, home, Path::new(".")),
        );
        Ok(manifest)
    }

    fn load_or_empty(path: &Path, home: &Path, root: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path, home, root)
        } else {
            Ok(Self::empty(root.to_path_buf(), home))
        }
    }

    fn resolve_game(&self, id: &str) -> Result<ResolvedGame> {
        let game = self
            .games
            .get(id)
            .ok_or_else(|| eyre!("unknown game `{}`", id))?;
        Ok(ResolvedGame::new(id, game, self))
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Game {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,

    install_dir: PathBuf,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    work_dir: Option<PathBuf>,

    launch: CommandSpec,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<PathText>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "is_false")]
    terminal: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    install: Option<CommandSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    update: Option<CommandSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    repair: Option<CommandSpec>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum CommandSpec {
    Program(PathText),
    Argv(Vec<PathText>),
}

impl CommandSpec {
    fn argv(&self) -> Result<Vec<PathText>> {
        let argv = match self {
            Self::Program(program) => vec![program.clone()],
            Self::Argv(argv) => argv.clone(),
        };
        if argv.is_empty() {
            bail!("command vector cannot be empty");
        }
        Ok(argv)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
struct PathText(String);

impl From<OsString> for PathText {
    fn from(value: OsString) -> Self {
        Self(value.to_string_lossy().into_owned())
    }
}

impl From<&str> for PathText {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for PathText {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl PathText {
    fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    fn to_string_lossy(&self) -> String {
        self.0.clone()
    }
}

#[derive(Debug, Serialize)]
struct ResolvedGame {
    id: String,
    title: String,
    source: Option<String>,
    install_dir: PathBuf,
    work_dir: PathBuf,
    launch: CommandSpec,
    args: Vec<PathText>,
    env: BTreeMap<String, String>,
    tags: Vec<String>,
    icon: Option<PathBuf>,
    terminal: bool,
    install: Option<CommandSpec>,
    update: Option<CommandSpec>,
    repair: Option<CommandSpec>,
}

impl ResolvedGame {
    fn new(id: &str, game: &Game, manifest: &Manifest) -> Self {
        let install_dir =
            normalize_path(&game.install_dir, &manifest.home, &manifest.root_resolved);
        let work_dir = game.work_dir.as_deref().map_or_else(
            || install_dir.clone(),
            |path| normalize_path(path, &manifest.home, &install_dir),
        );
        let icon = game
            .icon
            .as_deref()
            .map(|path| normalize_path(path, &manifest.home, &install_dir));
        Self {
            id: id.to_owned(),
            title: game.title.clone().unwrap_or_else(|| id.to_owned()),
            source: game.source.clone(),
            install_dir,
            work_dir,
            launch: game.launch.clone(),
            args: game.args.clone(),
            env: game.env.clone(),
            tags: game.tags.clone(),
            icon,
            terminal: game.terminal,
            install: game.install.clone(),
            update: game.update.clone(),
            repair: game.repair.clone(),
        }
    }

    fn launch_invocation(&self) -> Result<Invocation> {
        let mut argv = resolve_argv(&self.launch, &self.work_dir)?;
        argv.extend(self.args.clone());
        Ok(Invocation {
            cwd: self.work_dir.clone(),
            argv,
            env: self.env.clone(),
        })
    }

    fn hook_invocation(&self, hook: Hook) -> Result<Invocation> {
        let command = match hook {
            Hook::Install => self.install.as_ref(),
            Hook::Update => self.update.as_ref(),
            Hook::Repair => self.repair.as_ref(),
        }
        .ok_or_else(|| eyre!("`{}` has no {} hook", self.id, hook.name()))?;
        Ok(Invocation {
            cwd: self.work_dir.clone(),
            argv: resolve_argv(command, &self.work_dir)?,
            env: self.env.clone(),
        })
    }
}

#[derive(Debug)]
struct Invocation {
    cwd: PathBuf,
    argv: Vec<PathText>,
    env: BTreeMap<String, String>,
}

fn resolve_argv(command: &CommandSpec, cwd: &Path) -> Result<Vec<PathText>> {
    let mut argv = command.argv()?;
    if let Some(program) = argv.first_mut() {
        let path = program.as_path();
        if should_resolve_program(path) {
            *program = PathText(
                normalize_path(path, &home_dir()?, cwd)
                    .display()
                    .to_string(),
            );
        }
    }
    Ok(argv)
}

fn run_invocation(invocation: Invocation, dry_run: bool) -> Result<()> {
    let command_line = shellish(&invocation.argv);
    if dry_run {
        println!("cd {}", quote_path(&invocation.cwd));
        for (key, value) in &invocation.env {
            println!("export {}={}", key, shell_quote(value));
        }
        println!("{command_line}");
        return Ok(());
    }

    let (program, args) = invocation
        .argv
        .split_first()
        .ok_or_else(|| eyre!("empty command"))?;
    let status = Command::new(&program.0)
        .args(args.iter().map(|arg| arg.0.as_str()))
        .current_dir(&invocation.cwd)
        .envs(&invocation.env)
        .status()
        .with_context(|| format!("failed to spawn {}", program.to_string_lossy()))?;
    if !status.success() {
        bail!("command exited with {}", status);
    }
    Ok(())
}

fn list_games(manifest: &Manifest, json: bool) -> Result<()> {
    let games = manifest
        .games
        .iter()
        .map(|(id, game)| ResolvedGame::new(id, game, manifest))
        .collect::<Vec<_>>();
    if json {
        println!("{}", serde_json::to_string_pretty(&games)?);
        return Ok(());
    }

    for game in games {
        let source = game.source.as_deref().unwrap_or("-");
        let tags = if game.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", game.tags.join(","))
        };
        println!("{}\t{}\t{}{}", game.id, source, game.title, tags);
    }
    Ok(())
}

fn show_game(game: &ResolvedGame, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(game)?);
        return Ok(());
    }
    println!("id: {}", game.id);
    println!("title: {}", game.title);
    if let Some(source) = &game.source {
        println!("source: {source}");
    }
    println!("install_dir: {}", game.install_dir.display());
    println!("work_dir: {}", game.work_dir.display());
    println!("launch: {}", shellish(&game.launch_invocation()?.argv));
    if !game.tags.is_empty() {
        println!("tags: {}", game.tags.join(", "));
    }
    Ok(())
}

fn register_game(path: &Path, home: &Path, fallback_root: &Path, args: RegisterArgs) -> Result<()> {
    let mut manifest = Manifest::load(path, home, fallback_root)?;
    if manifest.games.contains_key(&args.id) && !args.force {
        bail!("`{}` already exists; pass --force to replace it", args.id);
    }

    let game = Game {
        title: args.title,
        source: args.source,
        install_dir: args.install_dir,
        work_dir: args.work_dir,
        launch: CommandSpec::Program(PathText::from(args.launch)),
        args: args.args.into_iter().map(PathText::from).collect(),
        env: args.env.into_iter().collect(),
        tags: args.tags,
        icon: args.icon,
        terminal: args.terminal,
        install: None,
        update: None,
        repair: None,
    };

    let _old = manifest.games.insert(args.id.clone(), game);
    manifest.save(path)?;
    println!("{}", args.id);
    Ok(())
}

fn upsert_manifest_game(
    path: &Path,
    home: &Path,
    root: &Path,
    id: String,
    game: Game,
) -> Result<()> {
    let mut manifest = Manifest::load_or_empty(path, home, root)?;
    let _old = manifest.games.insert(id, game);
    manifest.save(path)
}

async fn run_gog(action: GogAction, paths: &RuntimePaths) -> Result<()> {
    match action {
        GogAction::Auth { command } => run_gog_auth(command, &paths.home).await,
        GogAction::Info {
            product,
            os,
            language,
            json,
        } => {
            let mut client = GogClient::new(&paths.home)?;
            let selection = client.select_installer(&product, &os, &language).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&selection)?);
            } else {
                println!("id: {}", selection.product.id);
                println!("slug: {}", selection.product.slug);
                println!("title: {}", selection.product.title);
                println!("installer: {}", selection.installer.id);
                println!("os: {}", selection.installer.os);
                println!("language: {}", selection.installer.language);
                println!("version: {}", selection.installer.version);
                println!("size: {}", selection.file.size);
            }
            Ok(())
        }
        GogAction::Install {
            product,
            os,
            language,
            id,
            force,
            no_sandbox,
        } => {
            let mut client = GogClient::new(&paths.home)?;
            let selection = client.select_installer(&product, &os, &language).await?;
            let game_id = id.unwrap_or_else(|| flatten_slug(&selection.product.slug));
            let install = client
                .download_and_extract(&selection, &paths.root, &game_id, force, !no_sandbox)
                .await?;
            upsert_manifest_game(
                &paths.manifest_path,
                &paths.home,
                &paths.root,
                game_id.clone(),
                install.game,
            )?;
            println!(
                "installed {} {} at {}",
                selection.product.title,
                selection.installer.version,
                install.target.display()
            );
            println!("registered {game_id} in {}", paths.manifest_path.display());
            Ok(())
        }
    }
}

async fn run_store(action: StoreAction, paths: &RuntimePaths) -> Result<()> {
    match action {
        StoreAction::Gog { command } => run_gog(command, paths).await,
        StoreAction::Itch { command } => run_itch(command, paths).await,
    }
}

async fn run_gog_auth(command: GogAuthAction, home: &Path) -> Result<()> {
    let token_file = TokenFile::<GogToken>::new(home, "gog");
    match command {
        GogAuthAction::ImportLutris { path } => {
            let source = normalize_path(path, home, Path::new("."));
            let raw = fs::read_to_string(&source)
                .with_context(|| format!("failed to read {}", source.display()))?;
            let mut token: GogToken = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", source.display()))?;
            token.acquired_at = now_unix();
            token_file.write(&token)?;
            println!("imported GOG token from {}", source.display());
            Ok(())
        }
        GogAuthAction::Login { code } => {
            if let Some(code) = code {
                let code = extract_login_code(&code);
                let token = GogClient::exchange_code(&code).await?;
                token_file.write(&token)?;
                println!("stored GOG token");
            } else {
                println!("{}", gog_login_url());
                println!("After login, rerun with:");
                println!("gamectl store gog auth login --code '<redirect URL or code>'");
            }
            Ok(())
        }
        GogAuthAction::Status => {
            if !token_file.exists() {
                println!("missing: {}", token_file.path().display());
                println!("login: {}", gog_login_url());
                return Ok(());
            }
            let token = token_file.read()?;
            let expires_at = token.acquired_at.saturating_add(token.expires_in);
            let now = now_unix();
            let state = if expires_at <= now {
                "expired"
            } else {
                "valid"
            };
            println!("token: {state}");
            println!("path: {}", token_file.path().display());
            println!("expires_in: {}", expires_at.saturating_sub(now));
            if state == "expired" {
                println!(
                    "note: commands will try the stored refresh token; if refresh fails, rerun `gamectl store gog auth login`"
                );
            }
            if let Some(user_id) = token.user_id {
                println!("user_id: {user_id}");
            }
            Ok(())
        }
    }
}

#[derive(Debug)]
struct GogClient {
    http: HttpClient,
    home: PathBuf,
}

impl GogClient {
    fn new(home: &Path) -> Result<Self> {
        let http = HttpClient::builder().user_agent(USER_AGENT).build()?;
        Ok(Self {
            http,
            home: home.to_path_buf(),
        })
    }

    async fn exchange_code(code: &str) -> Result<GogToken> {
        let http = HttpClient::builder().user_agent(USER_AGENT).build()?;
        let mut token: GogToken = http
            .get("https://auth.gog.com/token")
            .query(&[
                ("client_id", GOG_CLIENT_ID),
                ("client_secret", GOG_CLIENT_SECRET),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", GOG_REDIRECT_URI),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        token.acquired_at = now_unix();
        Ok(token)
    }

    async fn select_installer(
        &mut self,
        product: &str,
        os: &str,
        language: &str,
    ) -> Result<GogSelection> {
        let product_id = self.resolve_product_id(product).await?;
        let product = self.product(product_id).await?;
        let downloads = product
            .downloads
            .clone()
            .ok_or_else(|| eyre!("GOG product {} has no download metadata", product.id))?;
        let installer = downloads
            .installers
            .into_iter()
            .find(|installer| installer.os == os && installer.language == language)
            .ok_or_else(|| {
                eyre!(
                    "no {os}/{language} offline installer for {} ({})",
                    product.title,
                    product.id
                )
            })?;
        let file = installer
            .files
            .first()
            .cloned()
            .ok_or_else(|| eyre!("installer {} has no downloadable files", installer.id))?;
        Ok(GogSelection {
            product,
            installer,
            file,
        })
    }

    async fn resolve_product_id(&mut self, product: &str) -> Result<u64> {
        if let Ok(id) = product.parse() {
            return Ok(id);
        }
        let query = product.replace(['-', '_'], " ");
        let search: CatalogSearch = self
            .http
            .get("https://catalog.gog.com/v1/catalog")
            .query(&[("limit", "10"), ("query", query.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let normalized = normalize_slug(product);
        let normalized_title = normalize_slug(product);
        let candidate = search
            .products
            .iter()
            .find(|item| normalize_slug(&item.slug) == normalized)
            .or_else(|| {
                search.products.iter().find(|item| {
                    item.title
                        .as_deref()
                        .is_some_and(|title| normalize_slug(title) == normalized_title)
                })
            })
            .ok_or_else(|| eyre!("could not resolve GOG product `{}`", product))?;
        candidate
            .id
            .parse()
            .wrap_err("catalog returned nonnumeric id")
    }

    async fn product(&self, product_id: u64) -> Result<GogProduct> {
        self.http
            .get(format!("https://api.gog.com/products/{product_id}"))
            .query(&[("expand", "downloads")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .wrap_err("failed to decode GOG product response")
    }

    async fn downlink(&mut self, product_id: u64, file_id: &str) -> Result<GogDownlink> {
        let token = self.valid_token().await?;
        let response = self
            .http
            .get(format!(
                "https://api.gog.com/products/{product_id}/downlink/installer/{file_id}"
            ))
            .bearer_auth(&token.access_token)
            .send()
            .await?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(token_expired_error());
        }
        response
            .error_for_status()?
            .json()
            .await
            .wrap_err("failed to decode GOG downlink response")
    }

    async fn valid_token(&mut self) -> Result<GogToken> {
        let token_file = TokenFile::<GogToken>::new(&self.home, "gog");
        let mut token = token_file
            .read()
            .wrap_err("GOG token missing or unreadable. Run `gamectl store gog auth import-lutris` or `gamectl store gog auth login`.")?;
        let now = now_unix();
        if token
            .acquired_at
            .saturating_add(token.expires_in)
            .saturating_sub(now)
            > 120
        {
            return Ok(token);
        }
        token = self.refresh_token(&token).await?;
        token_file.write(&token)?;
        Ok(token)
    }

    async fn refresh_token(&self, token: &GogToken) -> Result<GogToken> {
        let response = self
            .http
            .get("https://auth.gog.com/token")
            .query(&[
                ("client_id", GOG_CLIENT_ID),
                ("client_secret", GOG_CLIENT_SECRET),
                ("grant_type", "refresh_token"),
                ("refresh_token", token.refresh_token.as_str()),
            ])
            .send()
            .await?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(token_expired_error());
        }
        let mut refreshed: GogToken = response
            .error_for_status()?
            .json()
            .await
            .wrap_err("failed to decode refreshed GOG token")?;
        refreshed.acquired_at = now_unix();
        Ok(refreshed)
    }

    async fn download_and_extract(
        &mut self,
        selection: &GogSelection,
        root: &Path,
        game_id: &str,
        force: bool,
        sandbox: bool,
    ) -> Result<StoreInstall> {
        let cache = store_cache_dir(root, "gog");
        fs::create_dir_all(&cache)?;
        let downlink = self
            .downlink(selection.product.id, &selection.file.id)
            .await?;
        let checksum_xml = self
            .http
            .get(&downlink.checksum)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let checksum = parse_gog_checksum_xml(&checksum_xml)?;
        let installer = cache.join(&checksum.name);
        download_verified(&self.http, &downlink.downlink, &installer, &checksum).await?;

        let stage = store_stage_dir(&cache, game_id, &selection.installer.version);
        fs::create_dir_all(&stage)?;
        sandbox_extract_archive(&installer, &stage, "data/noarch/*", sandbox)?;

        let noarch = stage.join("data").join("noarch");
        if !noarch.is_dir() {
            bail!("installer did not contain data/noarch");
        }
        let target = root.join(game_id);
        let _backup = promote_install(&noarch, &target, force)?;
        let game = manifest_game_from_install(selection, game_id, &target);
        Ok(StoreInstall { target, game })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GogToken {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    token_type: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default = "now_unix")]
    acquired_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogSearch {
    products: Vec<CatalogProduct>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CatalogProduct {
    id: String,
    slug: String,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GogProduct {
    id: u64,
    title: String,
    slug: String,
    #[serde(default)]
    downloads: Option<GogDownloads>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GogDownloads {
    installers: Vec<GogInstaller>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GogInstaller {
    id: String,
    name: String,
    os: String,
    language: String,
    version: String,
    total_size: u64,
    files: Vec<GogInstallerFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GogInstallerFile {
    id: String,
    size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GogSelection {
    product: GogProduct,
    installer: GogInstaller,
    file: GogInstallerFile,
}

#[derive(Clone, Debug, Deserialize)]
struct GogDownlink {
    downlink: String,
    checksum: String,
}

#[derive(Clone, Debug)]
struct ArtifactChecksum {
    name: String,
    md5: String,
    total_size: u64,
}

#[derive(Debug)]
struct StoreInstall {
    target: PathBuf,
    game: Game,
}

#[derive(Debug, Deserialize)]
struct InstalledGogInfo {
    #[serde(default, rename = "playTasks")]
    play_tasks: Vec<InstalledPlayTask>,
}

#[derive(Debug, Deserialize)]
struct InstalledPlayTask {
    path: String,
    #[serde(default, rename = "isPrimary")]
    is_primary: bool,
    #[serde(default, rename = "type")]
    kind: String,
}

fn init_manifest(path: &Path, root: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite",
            path.display()
        );
    }

    let root_text = root.display().to_string();
    for subdir in ["library", "manifests", "bin", "saves"] {
        fs::create_dir_all(root.join(subdir))?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, starter_manifest(&root_text))?;
    println!("{}", path.display());
    Ok(())
}

fn print_paths(paths: &RuntimePaths) {
    println!("config: {}", paths.config_path.display());
    println!("manifest: {}", paths.manifest_path.display());
    println!("root: {}", paths.root.display());
}

fn write_desktop_entry(
    game: &ResolvedGame,
    manifest_path: &Path,
    output: Option<PathBuf>,
    print: bool,
    home: &Path,
) -> Result<()> {
    let entry = desktop_entry(game, manifest_path);
    if print {
        print!("{entry}");
        return Ok(());
    }

    let output = output.map_or_else(
        || {
            home.join(".local")
                .join("share")
                .join("applications")
                .join(format!("gamectl-{}.desktop", sanitize_filename(&game.id)))
        },
        |path| normalize_path(path, home, Path::new(".")),
    );
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, entry)?;
    println!("{}", output.display());
    Ok(())
}

fn desktop_entry(game: &ResolvedGame, manifest_path: &Path) -> String {
    let mut entry = String::new();
    entry.push_str("[Desktop Entry]\n");
    entry.push_str("Type=Application\n");
    entry.push_str("Name=");
    entry.push_str(&desktop_value(&game.title));
    entry.push('\n');
    entry.push_str("Exec=gamectl --manifest ");
    entry.push_str(&desktop_exec_arg(manifest_path));
    entry.push_str(" play ");
    entry.push_str(&desktop_exec_arg(Path::new(&game.id)));
    entry.push('\n');
    entry.push_str("Path=");
    entry.push_str(&desktop_value(&game.work_dir.display().to_string()));
    entry.push('\n');
    entry.push_str("Terminal=");
    entry.push_str(if game.terminal { "true" } else { "false" });
    entry.push('\n');
    if let Some(icon) = &game.icon {
        entry.push_str("Icon=");
        entry.push_str(&desktop_value(&icon.display().to_string()));
        entry.push('\n');
    }
    entry.push_str("Categories=Game;\n");
    entry
}

fn starter_manifest(root: &str) -> String {
    let root = toml_string(root);
    format!(
        r#"root = "{root}"

[games.example]
title = "Example Native Game"
source = "manual"
install_dir = "library/manual/example"
work_dir = "."
launch = "./example.x86_64"
args = []
tags = ["native"]
env = {{}}

# Hooks are optional argv arrays executed without a shell.
# install = ["lgogdownloader", "--download", "--game", "example"]
# update = ["butler", "update", "user/game", "library/itch/example"]
"#
    )
}

fn toml_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn parse_env_pair(value: &str) -> Result<(String, String), String> {
    let Some((key, value)) = value.split_once('=') else {
        return Err("expected KEY=VALUE".to_owned());
    };
    if key.is_empty() {
        return Err("environment key cannot be empty".to_owned());
    }
    Ok((key.to_owned(), value.to_owned()))
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug)]
struct TokenFile<T> {
    path: PathBuf,
    _marker: PhantomData<T>,
}

impl<T> TokenFile<T> {
    fn new(home: &Path, store: &str) -> Self {
        Self {
            path: xdg_state_home(home)
                .join("gamectl")
                .join("auth")
                .join(format!("{store}-token.json")),
            _marker: PhantomData,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn exists(&self) -> bool {
        self.path.exists()
    }
}

impl<T> TokenFile<T>
where
    T: DeserializeOwned,
{
    fn read(&self) -> Result<T> {
        if !self.exists() {
            bail!("token missing: {}", self.path.display());
        }
        let raw = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.path.display()))
    }
}

impl<T> TokenFile<T>
where
    T: Serialize,
{
    fn write(&self, token: &T) -> Result<()> {
        let mut raw = serde_json::to_string_pretty(token)?.into_bytes();
        raw.push(b'\n');
        write_private_bytes(&self.path, &raw)
    }
}

fn create_private_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to chmod 0700 {}", path.display()))
}

fn secure_private_file(path: &Path) -> Result<()> {
    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(bytes)?;
    secure_private_file(path)
}

fn token_expired_error() -> color_eyre::eyre::Report {
    eyre!(
        "GOG token expired or revoked. Run `gamectl store gog auth login` or `gamectl store gog auth import-lutris`."
    )
}

fn gog_login_url() -> String {
    format!(
        "https://auth.gog.com/auth?client_id={}&redirect_uri={}&response_type=code&layout=client2",
        percent_encode(GOG_CLIENT_ID),
        percent_encode(GOG_REDIRECT_URI)
    )
}

fn extract_login_code(raw: &str) -> String {
    raw.split_once("code=")
        .map_or(raw, |(_, tail)| tail.split('&').next().unwrap_or(tail))
        .to_owned()
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn parse_gog_checksum_xml(xml: &str) -> Result<ArtifactChecksum> {
    let header = xml
        .lines()
        .find(|line| line.trim_start().starts_with("<file "))
        .ok_or_else(|| eyre!("checksum XML did not contain a <file> header"))?;
    let name = xml_attr(header, "name")?;
    let md5 = xml_attr(header, "md5")?.to_ascii_lowercase();
    let total_size = xml_attr(header, "total_size")?
        .parse()
        .wrap_err("checksum XML total_size was not numeric")?;
    Ok(ArtifactChecksum {
        name,
        md5,
        total_size,
    })
}

fn xml_attr(line: &str, name: &str) -> Result<String> {
    let needle = format!("{name}=\"");
    let start = line
        .find(&needle)
        .ok_or_else(|| eyre!("checksum XML missing {name:?} attribute"))?
        + needle.len();
    let rest = &line[start..];
    let end = rest
        .find('"')
        .ok_or_else(|| eyre!("checksum XML has unterminated {name:?} attribute"))?;
    Ok(rest[..end].to_owned())
}

async fn download_verified(
    http: &HttpClient,
    url: &str,
    path: &Path,
    checksum: &ArtifactChecksum,
) -> Result<()> {
    if path.exists() && file_matches(path, checksum)? {
        println!("using cached {}", path.display());
        return Ok(());
    }

    let part = path.with_extension("part");
    let mut response = http.get(url).send().await?.error_for_status()?;
    let mut file = File::create(&part)?;
    let mut hasher = Md5::new();
    let mut total = 0_u64;
    loop {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        total += chunk.len() as u64;
    }
    file.flush()?;
    if total != checksum.total_size {
        bail!(
            "downloaded {} bytes for {}, expected {}",
            total,
            checksum.name,
            checksum.total_size
        );
    }
    let md5 = hex_lower(hasher.finalize().as_ref());
    if md5 != checksum.md5 {
        bail!(
            "MD5 mismatch for {}: got {}, expected {}",
            checksum.name,
            md5,
            checksum.md5
        );
    }
    fs::rename(part, path)?;
    Ok(())
}

fn file_matches(path: &Path, checksum: &ArtifactChecksum) -> Result<bool> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != checksum.total_size {
        return Ok(false);
    }
    Ok(file_md5(path)? == checksum.md5)
}

fn file_md5(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Md5::new();
    let mut buffer = vec![0_u8; 1024 * 256];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(hasher.finalize().as_ref()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn store_cache_dir(root: &Path, store: &str) -> PathBuf {
    root.join(".cache").join("gamectl").join(store)
}

fn store_stage_dir(cache: &Path, game_id: &str, version: &str) -> PathBuf {
    cache.join("stage").join(format!(
        "{}-{}-{}",
        game_id,
        version.replace('.', "_"),
        now_unix()
    ))
}

fn sandbox_extract_archive(
    archive: &Path,
    stage: &Path,
    payload_glob: &str,
    sandbox: bool,
) -> Result<()> {
    if sandbox {
        let status = Command::new("systemd-run")
            .args([
                "--user",
                "--wait",
                "--collect",
                "--quiet",
                "-p",
                "NoNewPrivileges=yes",
                "-p",
                "PrivateTmp=yes",
                "-p",
                "PrivateDevices=yes",
                "-p",
                "ProtectSystem=strict",
                "-p",
                "ProtectHome=read-only",
                "-p",
                "ProtectKernelTunables=yes",
                "-p",
                "ProtectKernelModules=yes",
                "-p",
                "ProtectControlGroups=yes",
                "-p",
                "RestrictSUIDSGID=yes",
                "-p",
                "LockPersonality=yes",
                "-p",
                "MemoryDenyWriteExecute=yes",
                "-p",
                "RestrictAddressFamilies=AF_UNIX",
                "-p",
                "CapabilityBoundingSet=",
                "-p",
            ])
            .arg(format!("ReadWritePaths={}", stage.display()))
            .arg("/usr/bin/bsdtar")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(stage)
            .arg(payload_glob)
            .status()
            .wrap_err("failed to spawn sandboxed bsdtar through systemd-run")?;
        if !status.success() {
            bail!("sandboxed installer extraction failed with {status}");
        }
        return Ok(());
    }

    let status = Command::new("bsdtar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(stage)
        .arg(payload_glob)
        .status()
        .wrap_err("failed to spawn bsdtar")?;
    if !status.success() {
        bail!("installer extraction failed with {status}");
    }
    Ok(())
}

fn promote_install(source: &Path, target: &Path, force: bool) -> Result<Option<PathBuf>> {
    if target.exists() {
        if !force {
            bail!(
                "{} already exists; pass --force to replace it",
                target.display()
            );
        }
        let backup = backup_path(target);
        fs::rename(target, &backup).with_context(|| {
            format!(
                "failed to move existing install {} to {}",
                target.display(),
                backup.display()
            )
        })?;
        fs::rename(source, target).with_context(|| {
            format!(
                "failed to install {}; previous install is at {}",
                target.display(),
                backup.display()
            )
        })?;
        println!("previous install moved to {}", backup.display());
        return Ok(Some(backup));
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(source, target)?;
    Ok(None)
}

fn backup_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("game")
        .to_owned();
    target.with_file_name(format!("{name}.backup-{}", now_unix()))
}

fn manifest_game_from_install(selection: &GogSelection, game_id: &str, target: &Path) -> Game {
    let (work_dir, launch) = installed_launch(target).unwrap_or_else(|| {
        (
            Some(PathBuf::from(".")),
            CommandSpec::Program(PathText::from("./start.sh")),
        )
    });
    let mut tags = vec!["native".to_owned(), "gog".to_owned()];
    tags.sort();
    tags.dedup();
    Game {
        title: Some(selection.product.title.clone()),
        source: Some("gog".to_owned()),
        install_dir: PathBuf::from(game_id),
        work_dir,
        launch,
        args: Vec::new(),
        env: BTreeMap::new(),
        tags,
        icon: Some(PathBuf::from("support/icon.png")),
        terminal: false,
        install: None,
        update: None,
        repair: None,
    }
}

fn installed_launch(target: &Path) -> Option<(Option<PathBuf>, CommandSpec)> {
    let game_dir = target.join("game");
    let infos = fs::read_dir(&game_dir).ok()?;
    for entry in infos.flatten() {
        let path = entry.path();
        let is_info = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                name.starts_with("goggame-")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("info"))
            });
        if !is_info {
            continue;
        }
        let raw = fs::read_to_string(path).ok()?;
        let info: InstalledGogInfo = serde_json::from_str(&raw).ok()?;
        let task = info
            .play_tasks
            .iter()
            .find(|task| task.is_primary && task.kind == "FileTask")
            .or_else(|| info.play_tasks.iter().find(|task| task.kind == "FileTask"))?;
        return Some((
            Some(PathBuf::from("game")),
            CommandSpec::Program(PathText::from(format!("./{}", task.path))),
        ));
    }
    None
}

fn normalize_slug(value: &str) -> String {
    value
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if matches!(c, '-' | '_' | ' ') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn flatten_slug(value: &str) -> String {
    normalize_slug(&value.replace('_', "-"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| eyre!("HOME is not set"))
}

fn xdg_config_home(home: &Path) -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
}

fn xdg_state_home(home: &Path) -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local").join("state"))
}

fn expand_path(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn normalize_path(path: impl AsRef<Path>, home: &Path, base: &Path) -> PathBuf {
    let path = path.as_ref();
    let expanded = path
        .to_str()
        .map_or_else(|| path.to_path_buf(), |text| expand_path(text, home));
    let normalized = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    lexical_clean(&normalized)
}

fn lexical_clean(path: &Path) -> PathBuf {
    let absolute = path.is_absolute();
    let mut parts = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_os_string()),
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => parts.push(part.to_os_string()),
            std::path::Component::ParentDir => {
                if parts
                    .last()
                    .is_some_and(|last| last.as_os_str() != OsStr::new(".."))
                {
                    let _ = parts.pop();
                } else if !absolute {
                    parts.push(OsString::from(".."));
                }
            }
        }
    }

    let mut cleaned = if absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    for part in parts {
        cleaned.push(part);
    }
    if cleaned.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        cleaned
    }
}

fn should_resolve_program(path: &Path) -> bool {
    path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
        || path.components().count() > 1
}

fn shellish(argv: &[PathText]) -> String {
    argv.iter()
        .map(|arg| shell_quote(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_path(path: &Path) -> String {
    shell_quote(&path.display().to_string())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn desktop_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "")
}

fn desktop_exec_arg(value: &Path) -> String {
    shell_quote(&value.display().to_string()).replace('%', "%%")
}

fn sanitize_filename(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct TestToken {
        value: String,
    }

    #[test]
    fn expands_home_paths() {
        let home = Path::new("/home/alice");
        assert_eq!(
            expand_path("~/Games", home),
            PathBuf::from("/home/alice/Games")
        );
        assert_eq!(expand_path("relative", home), PathBuf::from("relative"));
    }

    #[test]
    fn resolves_relative_paths_against_base() {
        let home = Path::new("/home/alice");
        let base = Path::new("/home/alice/Games");
        assert_eq!(
            normalize_path("library/foo", home, base),
            PathBuf::from("/home/alice/Games/library/foo")
        );
    }

    #[test]
    fn lexically_cleans_paths_without_touching_disk() {
        assert_eq!(
            lexical_clean(Path::new("/tmp/game/./bin/../run")),
            PathBuf::from("/tmp/game/run")
        );
        assert_eq!(
            lexical_clean(Path::new("../outer/./game")),
            PathBuf::from("../outer/game")
        );
    }

    #[test]
    fn parses_starter_manifest() -> Result<()> {
        let mut manifest: Manifest = toml::from_str(&starter_manifest(DEFAULT_ROOT))?;
        let home = PathBuf::from("/home/alice");
        manifest.home = home.clone();
        manifest.root_resolved = expand_path(DEFAULT_ROOT, &home);
        let game = manifest.resolve_game("example")?;
        assert_eq!(game.title, "Example Native Game");
        assert_eq!(
            game.install_dir,
            PathBuf::from("/home/alice/media/games/library/manual/example")
        );
        Ok(())
    }

    #[test]
    fn config_root_drives_relative_manifest_paths() {
        let home = Path::new("/home/alice");
        let config = GamectlConfig {
            root: Some(PathBuf::from("~/media/games")),
            manifest: Some(PathBuf::from("manifests/games.toml")),
        };
        let root = config.root_path(home).unwrap_or_default();

        assert_eq!(root, PathBuf::from("/home/alice/media/games"));
        assert_eq!(
            config.manifest_path(home, &root),
            Some(PathBuf::from(
                "/home/alice/media/games/manifests/games.toml"
            ))
        );
    }

    #[test]
    fn cli_parses_play() -> Result<()> {
        let cli = Cli::try_parse_from(["gamectl", "play", "songs-of-syx", "--", "--safe"])?;
        let Action::Play {
            game,
            dry_run,
            args,
        } = cli.command
        else {
            bail!("play did not parse as Action::Play");
        };
        assert_eq!(game, "songs-of-syx");
        assert!(!dry_run);
        assert_eq!(args, vec![OsString::from("--safe")]);
        Ok(())
    }

    #[test]
    fn cli_parses_hook() -> Result<()> {
        let cli = Cli::try_parse_from(["gamectl", "hook", "songs-of-syx", "update"])?;
        let Action::Hook { game, hook, .. } = cli.command else {
            bail!("hook did not parse as Action::Hook");
        };
        assert_eq!(game, "songs-of-syx");
        assert!(matches!(hook, Hook::Update));
        Ok(())
    }

    #[test]
    fn cli_parses_store_backends() -> Result<()> {
        let cli = Cli::try_parse_from(["gamectl", "store", "itch", "auth", "status"])?;
        let Action::Store {
            command:
                StoreAction::Itch {
                    command:
                        ItchAction::Auth {
                            command: itch::ItchAuthAction::Status { .. },
                        },
                },
        } = cli.command
        else {
            bail!("store itch auth status did not parse as nested itch auth status");
        };
        Ok(())
    }

    #[test]
    fn token_file_writes_private_json() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let token = TestToken {
            value: "secret".to_owned(),
        };
        let token_file = TokenFile::<TestToken>::new(dir.path(), "test");
        token_file.write(&token)?;

        assert_eq!(
            token_file.path(),
            dir.path()
                .join(".local")
                .join("state")
                .join("gamectl")
                .join("auth")
                .join("test-token.json")
        );
        let parent = token_file.path().parent().unwrap_or(dir.path());
        let parent_mode = fs::metadata(parent)?.permissions().mode() & 0o777;
        assert_eq!(parent_mode, 0o700);
        let mode = fs::metadata(token_file.path())?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(token_file.read()?, token);
        Ok(())
    }

    #[test]
    fn checksum_match_rejects_wrong_size_or_md5() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let payload = dir.path().join("payload.bin");
        fs::write(&payload, b"abc")?;
        let checksum = ArtifactChecksum {
            name: "payload.bin".to_owned(),
            md5: file_md5(&payload)?,
            total_size: 3,
        };
        assert!(file_matches(&payload, &checksum)?);

        let wrong_size = ArtifactChecksum {
            total_size: 4,
            ..checksum.clone()
        };
        assert!(!file_matches(&payload, &wrong_size)?);

        let wrong_md5 = ArtifactChecksum {
            md5: "00000000000000000000000000000000".to_owned(),
            ..checksum
        };
        assert!(!file_matches(&payload, &wrong_md5)?);
        Ok(())
    }

    #[test]
    fn manifest_upsert_preserves_unrelated_games() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let manifest = dir.path().join("games.toml");
        let root = dir.path().join("games");
        upsert_manifest_game(
            &manifest,
            dir.path(),
            &root,
            "alpha".to_owned(),
            test_game("alpha"),
        )?;
        upsert_manifest_game(
            &manifest,
            dir.path(),
            &root,
            "beta".to_owned(),
            test_game("beta"),
        )?;

        let loaded = Manifest::load(&manifest, dir.path(), &root)?;
        assert!(loaded.games.contains_key("alpha"));
        assert!(loaded.games.contains_key("beta"));
        Ok(())
    }

    #[test]
    fn promote_install_backs_up_existing_target() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("new");
        let target = dir.path().join("game");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&target)?;
        fs::write(source.join("version"), b"new")?;
        fs::write(target.join("version"), b"old")?;

        let Some(backup) = promote_install(&source, &target, true)? else {
            bail!("missing backup path");
        };
        assert_eq!(fs::read(target.join("version"))?, b"new");
        assert_eq!(fs::read(backup.join("version"))?, b"old");
        assert!(!source.exists());
        Ok(())
    }

    fn test_game(id: &str) -> Game {
        Game {
            title: Some(id.to_owned()),
            source: Some("test".to_owned()),
            install_dir: PathBuf::from(id),
            work_dir: None,
            launch: CommandSpec::Program(PathText::from("./run")),
            args: Vec::new(),
            env: BTreeMap::new(),
            tags: Vec::new(),
            icon: None,
            terminal: false,
            install: None,
            update: None,
            repair: None,
        }
    }
}

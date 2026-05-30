use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

use clap::Subcommand;
use color_eyre::eyre::{Context, Result, bail, eyre};
use reqwest::Client as HttpClient;
use serde_json::{Value, json};

use crate::{
    CommandSpec, Game, Manifest, PathText, RuntimePaths, USER_AGENT, flatten_slug, normalize_path,
    upsert_manifest_game,
};

const ITCH_ADDRESS: &str = "https://itch.io";
const ITCH_BUTLER_ARCHIVE: &str =
    "https://broth.itch.zone/butler/linux-amd64/LATEST/archive/default";
const ITCH_BUTLER_LATEST: &str = "https://broth.itch.zone/butler/linux-amd64/LATEST";
const ITCH_LUTRIS_API_KEY: &str = "~/.cache/lutris/itchio/api-key";
const ITCH_LUTRIS_COOKIE_JAR: &str = "~/.cache/lutris/.itchio.auth";
const ITCH_LUTRIS_GAMES: &str = "~/.config/lutris/games";
const BUTLER_DB_FILES: &[&str] = &["butler.db", "butler.db-wal", "butler.db-shm"];

#[derive(Debug, Subcommand)]
pub(crate) enum ItchAction {
    /// Manage itch.io authentication through butlerd.
    Auth {
        #[command(subcommand)]
        command: ItchAuthAction,
    },

    /// Download and install/update the first-party butler binary.
    Bootstrap {
        #[arg(long)]
        force: bool,
    },

    /// List owned itch.io games through butlerd.
    Library {
        #[arg(long)]
        fresh: bool,

        #[arg(long)]
        json: bool,

        /// Run butlerd directly without the systemd sandbox.
        #[arg(long)]
        no_sandbox: bool,
    },

    /// Import installed itch.io games from Lutris into the manifest.
    ImportLutris {
        #[arg(long, default_value = ITCH_LUTRIS_GAMES)]
        games_dir: PathBuf,

        #[arg(long)]
        force: bool,
    },

    /// Download, install, and manifest-register one owned itch.io game.
    Install {
        game: String,

        #[arg(long)]
        id: Option<String>,

        #[arg(long)]
        upload: Option<u64>,

        #[arg(long)]
        fresh: bool,

        #[arg(long)]
        force: bool,

        /// Run butlerd directly without the systemd sandbox.
        #[arg(long)]
        no_sandbox: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ItchAuthAction {
    /// Import an existing Lutris itch.io API key into butlerd.
    ImportLutris {
        #[arg(long, default_value = ITCH_LUTRIS_API_KEY)]
        path: PathBuf,

        /// Run butlerd directly without the systemd sandbox.
        #[arg(long)]
        no_sandbox: bool,
    },

    /// Store an itch.io API key in butlerd's private gamectl database.
    LoginKey {
        #[arg(long)]
        key: Option<String>,

        #[arg(long, value_name = "PATH")]
        key_file: Option<PathBuf>,

        /// Run butlerd directly without the systemd sandbox.
        #[arg(long)]
        no_sandbox: bool,
    },

    /// Show remembered butlerd profiles.
    Status {
        /// Run butlerd directly without the systemd sandbox.
        #[arg(long)]
        no_sandbox: bool,
    },
}

pub(crate) async fn run_itch(action: ItchAction, paths: &RuntimePaths) -> Result<()> {
    match action {
        ItchAction::Auth { command } => run_itch_auth(command, paths).await,
        ItchAction::Bootstrap { force } => {
            let butler = ensure_butler(&paths.home, force).await?;
            println!("{}", butler.display());
            Ok(())
        }
        ItchAction::Library {
            fresh,
            json,
            no_sandbox,
        } => {
            let mut butler = Butlerd::spawn(paths, !no_sandbox).await?;
            let profile_id = butler.use_first_profile()?;
            let games = butler.owned_keys(profile_id, fresh)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&games)?);
            } else {
                for key in &games {
                    let game = key
                        .get("game")
                        .ok_or_else(|| eyre!("download key did not contain game"))?;
                    println!(
                        "{}\t{}",
                        json_u64(game, "id")?,
                        json_string(game, "title").unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        ItchAction::ImportLutris { games_dir, force } => {
            import_lutris_itch_games(paths, games_dir, force)
        }
        ItchAction::Install {
            game,
            id,
            upload,
            fresh,
            force,
            no_sandbox,
        } => {
            let mut butler = Butlerd::spawn(paths, !no_sandbox).await?;
            let profile_id = butler.use_first_profile()?;
            let owned = butler.owned_keys(profile_id, fresh)?;
            let (game_value, game_id, title) = find_itch_game(&owned, &game)?;
            let upload_value = butler.select_upload(game_id, upload, fresh)?;
            let location_id = butler.ensure_install_location(&paths.root)?;
            let game_slug = id.unwrap_or_else(|| flatten_slug(&title));
            let staging = itch_cache_home(&paths.home).join("stage").join(&game_slug);
            fs::create_dir_all(&staging)?;
            let queued = butler.call(
                "Install.Queue",
                json!({
                    "reason": "install",
                    "installLocationId": location_id,
                    "game": game_value,
                    "upload": upload_value,
                    "ignoreInstallers": true,
                    "stagingFolder": staging,
                }),
            )?;
            let install_id = json_string(&queued, "id")?;
            let staging_folder = json_string(&queued, "stagingFolder")?;
            let installed = butler.call(
                "Install.Perform",
                json!({
                    "id": install_id,
                    "stagingFolder": staging_folder,
                }),
            )?;
            let cave_id = json_string(&installed, "caveId")?;
            let cave = butler.call("Fetch.Cave", json!({ "caveId": cave_id }))?;
            let target = cave_install_folder(&cave)?;
            let manifest_game = manifest_game_from_itch(&title, &game_slug, &paths.root, &target)?;
            if !force
                && Manifest::load_or_empty(&paths.manifest_path, &paths.home, &paths.root)?
                    .games
                    .contains_key(&game_slug)
            {
                bail!("`{game_slug}` already exists; pass --force to replace it");
            }
            upsert_manifest_game(
                &paths.manifest_path,
                &paths.home,
                &paths.root,
                game_slug.clone(),
                manifest_game,
            )?;
            println!("installed {title} at {}", target.display());
            println!(
                "registered {game_slug} in {}",
                paths.manifest_path.display()
            );
            Ok(())
        }
    }
}

async fn run_itch_auth(command: ItchAuthAction, paths: &RuntimePaths) -> Result<()> {
    match command {
        ItchAuthAction::ImportLutris { path, no_sandbox } => {
            let path = normalize_path(path, &paths.home, Path::new("."));
            if !path.exists() {
                if let Some(preserved) =
                    preserve_lutris_itch_auth(paths, PathBuf::from(ITCH_LUTRIS_COOKIE_JAR))?
                {
                    bail!(
                        "Lutris itch.io API key not found at {}; preserved Lutris browser cookie jar at {}. Butlerd cannot consume browser cookies directly; create an itch API key and run `gamectl itch auth login-key --key-file PATH`.",
                        path.display(),
                        preserved.display()
                    );
                }
                bail!(
                    "Lutris itch.io API key not found at {}; Lutris appears to have no reusable API key or browser cookie jar. Create an itch API key and run `gamectl itch auth login-key --key-file PATH`.",
                    path.display()
                );
            }
            let api_key = read_secret_file(&path)?;
            let mut butler = Butlerd::spawn(paths, !no_sandbox).await?;
            let profile = butler.login_api_key(&api_key)?;
            println!(
                "imported itch.io profile {}",
                json_string(&profile, "username").unwrap_or_else(|_| json_u64(&profile, "id")
                    .map_or_else(|_| "unknown".to_owned(), |id| id.to_string()))
            );
            Ok(())
        }
        ItchAuthAction::LoginKey {
            key,
            key_file,
            no_sandbox,
        } => {
            let api_key = match (key, key_file) {
                (Some(key), None) => key,
                (None, Some(path)) => {
                    read_secret_file(&normalize_path(path, &paths.home, Path::new(".")))?
                }
                (Some(_), Some(_)) => bail!("pass either --key or --key-file, not both"),
                (None, None) => bail!(
                    "pass --key-file PATH; avoiding interactive secret prompts keeps this agent-friendly"
                ),
            };
            let mut butler = Butlerd::spawn(paths, !no_sandbox).await?;
            let profile = butler.login_api_key(&api_key)?;
            println!(
                "stored itch.io profile {}",
                json_string(&profile, "username").unwrap_or_else(|_| json_u64(&profile, "id")
                    .map_or_else(|_| "unknown".to_owned(), |id| id.to_string()))
            );
            Ok(())
        }
        ItchAuthAction::Status { no_sandbox } => {
            print_itch_auth_paths(paths);
            let mut butler = Butlerd::spawn(paths, !no_sandbox).await?;
            let profiles = butler.call("Profile.List", json!({}))?;
            let profiles = profiles
                .get("profiles")
                .and_then(Value::as_array)
                .ok_or_else(|| eyre!("Profile.List returned no profiles array"))?;
            if profiles.is_empty() {
                println!("profiles: none");
                return Ok(());
            }
            for profile in profiles {
                println!(
                    "{}\t{}",
                    json_u64(profile, "id")?,
                    json_string(profile, "username").unwrap_or_default()
                );
            }
            Ok(())
        }
    }
}

#[derive(Debug)]
struct Butlerd {
    child: Child,
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    state: PathBuf,
    next_id: u64,
}

impl Butlerd {
    async fn spawn(paths: &RuntimePaths, sandbox: bool) -> Result<Self> {
        let butler = ensure_butler(&paths.home, false).await?;
        let state = itch_state_home(&paths.home);
        let cache = itch_cache_home(&paths.home);
        secure_butler_state(&state)?;
        fs::create_dir_all(&cache)?;
        fs::create_dir_all(&paths.root)?;

        let mut command = if sandbox {
            sandboxed_butler_command(&butler, paths, &state, &cache)
        } else {
            direct_butler_command(&butler, &state)
        };
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .wrap_err("failed to spawn butlerd")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre!("butlerd stdout was not captured"))?;
        let mut stdout = BufReader::new(stdout);
        let mut line = String::new();
        let listen = loop {
            line.clear();
            let n = stdout
                .read_line(&mut line)
                .wrap_err("failed to read butlerd listen notification")?;
            if n == 0 {
                let status = child.wait().ok();
                bail!("butlerd exited before listen notification: {status:?}");
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) == Some("butlerd/listen-notification") {
                break value;
            }
        };
        let address = listen
            .get("tcp")
            .and_then(|tcp| tcp.get("address"))
            .and_then(Value::as_str)
            .ok_or_else(|| eyre!("butlerd listen notification did not contain tcp.address"))?;
        let secret = listen
            .get("secret")
            .and_then(Value::as_str)
            .ok_or_else(|| eyre!("butlerd listen notification did not contain secret"))?;
        let writer = TcpStream::connect(address)
            .with_context(|| format!("failed to connect to butlerd at {address}"))?;
        let reader = BufReader::new(writer.try_clone()?);
        let mut this = Self {
            child,
            reader,
            writer,
            state,
            next_id: 1,
        };
        let _auth = this.call("Meta.Authenticate", json!({ "secret": secret }))?;
        secure_butler_state(&this.state)?;
        Ok(this)
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        serde_json::to_writer(&mut self.writer, &request)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .reader
                .read_line(&mut line)
                .with_context(|| format!("failed to read butlerd response for {method}"))?;
            if n == 0 {
                bail!("butlerd closed connection while waiting for {method}");
            }
            let message: Value = serde_json::from_str(&line).with_context(|| {
                format!("butlerd emitted invalid JSON while waiting for {method}")
            })?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                maybe_print_butler_notification(&message);
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown butlerd error");
                bail!("{method} failed: {text}");
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| eyre!("{method} response lacked result"));
        }
    }

    fn login_api_key(&mut self, api_key: &str) -> Result<Value> {
        let response = self.call("Profile.LoginWithAPIKey", json!({ "apiKey": api_key }))?;
        response
            .get("profile")
            .cloned()
            .ok_or_else(|| eyre!("Profile.LoginWithAPIKey returned no profile"))
    }

    fn use_first_profile(&mut self) -> Result<u64> {
        let response = self.call("Profile.List", json!({}))?;
        let profiles = response
            .get("profiles")
            .and_then(Value::as_array)
            .ok_or_else(|| eyre!("Profile.List returned no profiles array"))?;
        let profile = profiles.first().ok_or_else(|| {
            eyre!("no itch.io profile saved; run `gamectl itch auth import-lutris` or `gamectl itch auth login-key --key-file PATH`")
        })?;
        let profile_id = json_u64(profile, "id")?;
        let _validated = self.call("Profile.UseSavedLogin", json!({ "profileId": profile_id }))?;
        Ok(profile_id)
    }

    fn owned_keys(&mut self, profile_id: u64, fresh: bool) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut cursor: Option<Value> = None;
        loop {
            let mut params = json!({
                "profileId": profile_id,
                "limit": 100,
                "fresh": fresh,
            });
            if let Some(cursor) = cursor.take() {
                params["cursor"] = cursor;
            }
            let page = self.call("Fetch.ProfileOwnedKeys", params)?;
            let page_items = page
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| eyre!("Fetch.ProfileOwnedKeys returned no items array"))?;
            items.extend(page_items.iter().cloned());
            cursor = page.get("nextCursor").cloned();
            if cursor.is_none() {
                break;
            }
        }
        Ok(items)
    }

    fn select_upload(
        &mut self,
        game_id: u64,
        upload_id: Option<u64>,
        fresh: bool,
    ) -> Result<Value> {
        let response = self.call(
            "Fetch.GameUploads",
            json!({
                "gameId": game_id,
                "compatible": true,
                "fresh": fresh,
            }),
        )?;
        let uploads = response
            .get("uploads")
            .and_then(Value::as_array)
            .ok_or_else(|| eyre!("Fetch.GameUploads returned no uploads array"))?;
        if let Some(upload_id) = upload_id {
            return uploads
                .iter()
                .find(|upload| json_u64(upload, "id").ok() == Some(upload_id))
                .cloned()
                .ok_or_else(|| {
                    eyre!("no compatible itch.io upload {upload_id} for game {game_id}")
                });
        }
        let candidates = uploads
            .iter()
            .filter(|upload| {
                upload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    == "default"
                    && !upload.get("demo").and_then(Value::as_bool).unwrap_or(false)
                    && !upload
                        .get("preorder")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [upload] => Ok(upload.clone()),
            [] => bail!("no compatible default itch.io uploads for game {game_id}"),
            many => {
                eprintln!("multiple compatible itch.io uploads; select one with --upload:");
                for upload in many {
                    eprintln!(
                        "{}\t{}\t{}",
                        json_u64(upload, "id")?,
                        json_string(upload, "filename").unwrap_or_default(),
                        json_string(upload, "displayName").unwrap_or_default()
                    );
                }
                bail!("ambiguous itch.io upload")
            }
        }
    }

    fn ensure_install_location(&mut self, root: &Path) -> Result<String> {
        let root = root.display().to_string();
        let locations = self.call("Install.Locations.List", json!({}))?;
        if let Some(existing) = locations
            .get("installLocations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|location| location.get("path").and_then(Value::as_str) == Some(root.as_str()))
        {
            return json_string(existing, "id");
        }
        let created = self.call(
            "Install.Locations.Add",
            json!({
                "id": "gamectl-root",
                "path": root,
            }),
        )?;
        json_string(
            created
                .get("installLocation")
                .ok_or_else(|| eyre!("Install.Locations.Add returned no installLocation"))?,
            "id",
        )
    }
}

impl Drop for Butlerd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = secure_butler_state(&self.state);
    }
}

fn sandboxed_butler_command(
    butler: &Path,
    paths: &RuntimePaths,
    state: &Path,
    cache: &Path,
) -> Command {
    let mut command = Command::new("systemd-run");
    let _command = command.args([
        "--user",
        "--pipe",
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
        "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
        "-p",
        "CapabilityBoundingSet=",
        "-p",
        "RestrictSUIDSGID=yes",
        "-p",
        "UMask=0077",
        "-p",
        "LockPersonality=yes",
        "-p",
        "SystemCallArchitectures=native",
        "-p",
    ]);
    let _command = command.arg(format!("ReadWritePaths={}", paths.root.display()));
    let _command = command.args(["-p"]);
    let _command = command.arg(format!("ReadWritePaths={}", state.display()));
    let _command = command.args(["-p"]);
    let _command = command.arg(format!("ReadWritePaths={}", cache.display()));
    add_butler_daemon_args(&mut command, butler, state);
    command
}

fn direct_butler_command(butler: &Path, state: &Path) -> Command {
    let mut command = Command::new(butler);
    add_butler_daemon_tail(&mut command, state);
    command
}

fn add_butler_daemon_args(command: &mut Command, butler: &Path, state: &Path) {
    let _command = command.arg(butler);
    add_butler_daemon_tail(command, state);
}

fn add_butler_daemon_tail(command: &mut Command, state: &Path) {
    let _command = command.args([
        "daemon",
        "--json",
        "--transport",
        "tcp",
        "--keep-alive",
        "--address",
        ITCH_ADDRESS,
        "--user-agent",
        USER_AGENT,
        "--destiny-pid",
    ]);
    let _command = command.arg(std::process::id().to_string());
    let _command = command.arg("--dbpath");
    let _command = command.arg(butler_db_path(state));
}

async fn ensure_butler(home: &Path, force: bool) -> Result<PathBuf> {
    let bin = itch_butler_bin(home);
    if bin.exists() && !force {
        return Ok(bin);
    }
    let http = HttpClient::builder().user_agent(USER_AGENT).build()?;
    let version = http
        .get(ITCH_BUTLER_LATEST)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?
        .trim()
        .to_owned();
    if version.is_empty() {
        bail!("empty butler version from {ITCH_BUTLER_LATEST}");
    }
    let cache = itch_cache_home(home);
    fs::create_dir_all(&cache)?;
    let zip = cache.join(format!("butler-{version}.zip"));
    let url = ITCH_BUTLER_ARCHIVE;
    if !zip.exists() || force {
        let bytes = http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        fs::write(&zip, bytes).with_context(|| format!("failed to write {}", zip.display()))?;
    }
    let extract_dir = cache.join(format!("butler-{version}"));
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&extract_dir)?;
    let status = Command::new("unzip")
        .arg("-oq")
        .arg(&zip)
        .arg("-d")
        .arg(&extract_dir)
        .status()
        .wrap_err("failed to spawn unzip for butler bootstrap")?;
    if !status.success() {
        bail!("unzip failed with {status}");
    }
    let extracted = extract_dir.join("butler");
    if !extracted.is_file() {
        bail!("butler archive did not contain executable `butler`");
    }
    if let Some(parent) = bin.parent() {
        fs::create_dir_all(parent)?;
    }
    let _bytes = fs::copy(&extracted, &bin)?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    fs::write(itch_butler_version(home), version)?;
    Ok(bin)
}

fn read_secret_file(path: &Path) -> Result<String> {
    let secret = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?
        .trim()
        .to_owned();
    if secret.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(secret)
}

fn print_itch_auth_paths(paths: &RuntimePaths) {
    let state = itch_state_home(&paths.home);
    println!("state_dir: {}", state.display());
    println!("profile_db: {}", butler_db_path(&state).display());
    println!("cache_dir: {}", itch_cache_home(&paths.home).display());

    let preserved = preserved_lutris_cookie_path(&paths.home);
    if preserved.exists() {
        println!("preserved_lutris_cookie_jar: {}", preserved.display());
    }
}

fn preserve_lutris_itch_auth(paths: &RuntimePaths, cookie_jar: PathBuf) -> Result<Option<PathBuf>> {
    let source = normalize_path(cookie_jar, &paths.home, Path::new("."));
    if !source.exists() {
        return Ok(None);
    }

    let target = preserved_lutris_cookie_path(&paths.home);
    copy_private_file(&source, &target)?;
    Ok(Some(target))
}

fn preserved_lutris_cookie_path(home: &Path) -> PathBuf {
    itch_state_home(home).join("lutris").join("itchio.auth")
}

fn copy_private_file(source: &Path, target: &Path) -> Result<()> {
    let raw = fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    write_private_file(target, &raw)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
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

fn secure_butler_state(state: &Path) -> Result<()> {
    create_private_dir_all(state)?;
    for name in BUTLER_DB_FILES {
        secure_private_file(&state.join(name))?;
    }
    Ok(())
}

fn butler_db_path(state: &Path) -> PathBuf {
    state.join("butler.db")
}

fn import_lutris_itch_games(paths: &RuntimePaths, games_dir: PathBuf, force: bool) -> Result<()> {
    let games_dir = normalize_path(games_dir, &paths.home, Path::new("."));
    let mut imported = 0_usize;
    for entry in fs::read_dir(&games_dir)
        .with_context(|| format!("failed to read {}", games_dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(OsStr::to_str) != Some("yml") {
            continue;
        }
        let Some(lutris) = LutrisItchGame::parse(&path)? else {
            continue;
        };
        let game_id = flatten_slug(&lutris.slug);
        if !force
            && Manifest::load_or_empty(&paths.manifest_path, &paths.home, &paths.root)?
                .games
                .contains_key(&game_id)
        {
            bail!("`{game_id}` already exists; pass --force to replace it");
        }
        let install_dir = relative_to(&lutris.install_dir, &paths.root)
            .unwrap_or_else(|| lutris.install_dir.clone());
        let work_dir = relative_to(&lutris.work_dir, &lutris.install_dir);
        let launch =
            relative_to(&lutris.launch, &lutris.work_dir).unwrap_or_else(|| lutris.launch.clone());
        let game = Game {
            title: Some(lutris.name),
            source: Some("itchio".to_owned()),
            install_dir,
            work_dir,
            launch: CommandSpec::Program(PathText::from(format!("./{}", launch.display()))),
            args: Vec::new(),
            env: BTreeMap::new(),
            tags: vec!["itchio".to_owned(), "native".to_owned()],
            icon: None,
            terminal: false,
            install: None,
            update: None,
            repair: None,
        };
        upsert_manifest_game(
            &paths.manifest_path,
            &paths.home,
            &paths.root,
            game_id.clone(),
            game,
        )?;
        println!("imported {game_id}");
        imported = imported.saturating_add(1);
    }
    println!("imported: {imported}");
    Ok(())
}

#[derive(Debug)]
struct LutrisItchGame {
    name: String,
    slug: String,
    install_dir: PathBuf,
    work_dir: PathBuf,
    launch: PathBuf,
}

impl LutrisItchGame {
    fn parse(path: &Path) -> Result<Option<Self>> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if !raw.lines().any(|line| line.trim() == "service: itchio") {
            return Ok(None);
        }
        let name = yaml_scalar(&raw, "name").unwrap_or_else(|| {
            path.file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or("itchio-game")
                .to_owned()
        });
        let slug = yaml_scalar(&raw, "slug").unwrap_or_else(|| flatten_slug(&name));
        let launch = raw
            .lines()
            .find_map(|line| line.strip_prefix("  exe: "))
            .map(unquote_scalar)
            .map(PathBuf::from)
            .ok_or_else(|| eyre!("{} did not contain game.exe", path.display()))?;
        let work_dir = launch
            .parent()
            .ok_or_else(|| eyre!("{} has launcher without parent dir", path.display()))?
            .to_path_buf();
        let install_dir = work_dir.clone();
        Ok(Some(Self {
            name,
            slug,
            install_dir,
            work_dir,
            launch,
        }))
    }
}

fn yaml_scalar(raw: &str, key: &str) -> Option<String> {
    let needle = format!("{key}: ");
    raw.lines()
        .find_map(|line| line.strip_prefix(&needle))
        .map(unquote_scalar)
}

fn unquote_scalar(value: &str) -> String {
    value.trim().trim_matches('"').trim_matches('\'').to_owned()
}

fn find_itch_game(owned: &[Value], query: &str) -> Result<(Value, u64, String)> {
    let normalized = flatten_slug(query);
    let game = owned
        .iter()
        .filter_map(|key| key.get("game"))
        .find(|game| {
            json_u64(game, "id").is_ok_and(|id| id.to_string() == query)
                || json_string(game, "title").is_ok_and(|title| flatten_slug(&title) == normalized)
                || game
                    .get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| itch_url_slug(url) == normalized)
        })
        .ok_or_else(|| eyre!("could not find owned itch.io game `{query}`"))?;
    let id = json_u64(game, "id")?;
    let title = json_string(game, "title")?;
    Ok((game.clone(), id, title))
}

fn itch_url_slug(url: &str) -> String {
    flatten_slug(url.trim_end_matches('/').rsplit('/').next().unwrap_or(url))
}

fn manifest_game_from_itch(title: &str, game_id: &str, root: &Path, target: &Path) -> Result<Game> {
    let (work_dir, launch) = discover_native_launch(target)?;
    let install_dir = relative_to(target, root).unwrap_or_else(|| target.to_path_buf());
    let work_dir = relative_to(&work_dir, target);
    Ok(Game {
        title: Some(title.to_owned()),
        source: Some("itchio".to_owned()),
        install_dir,
        work_dir,
        launch: CommandSpec::Program(PathText::from(format!("./{}", launch.display()))),
        args: Vec::new(),
        env: BTreeMap::new(),
        tags: vec!["itchio".to_owned(), "native".to_owned()],
        icon: None,
        terminal: false,
        install: Some(CommandSpec::Argv(vec![
            PathText::from("gamectl"),
            PathText::from("itch"),
            PathText::from("install"),
            PathText::from(game_id.to_owned()),
            PathText::from("--force"),
        ])),
        update: None,
        repair: None,
    })
}

fn discover_native_launch(target: &Path) -> Result<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    collect_executables(target, target, 0, &mut candidates)?;
    candidates.sort_by_key(|path| executable_score(path.as_path()));
    let launch = candidates.into_iter().next().ok_or_else(|| {
        eyre!(
            "could not identify a native Linux launcher under {}",
            target.display()
        )
    })?;
    let work_dir = launch.parent().unwrap_or(target).to_path_buf();
    let relative_launch = relative_to(&launch, &work_dir).unwrap_or(launch.clone());
    Ok((work_dir, relative_launch))
}

fn collect_executables(
    root: &Path,
    dir: &Path,
    depth: usize,
    candidates: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth > 2 {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
        if name == ".itch" || name.starts_with("lib") {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_executables(root, &path, depth.saturating_add(1), candidates)?;
        } else if file_type.is_file() && is_executable(&path)? {
            let relative = relative_to(&path, root).unwrap_or_else(|| path.clone());
            if !relative.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|part| part == ".itch" || part == "itchupload")
            }) {
                candidates.push(path);
            }
        }
    }
    Ok(())
}

fn executable_score(path: &Path) -> (u8, usize, String) {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let extension = path.extension().and_then(OsStr::to_str);
    let rank = if name.starts_with("run") || name.starts_with("start") {
        0
    } else if extension.is_some_and(|extension| {
        extension.eq_ignore_ascii_case("sh") || extension.eq_ignore_ascii_case("x86_64")
    }) {
        1
    } else {
        2
    };
    (rank, path.components().count(), name)
}

fn is_executable(path: &Path) -> Result<bool> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

fn cave_install_folder(cave_response: &Value) -> Result<PathBuf> {
    cave_response
        .get("cave")
        .and_then(|cave| cave.get("installInfo"))
        .and_then(|info| info.get("installFolder"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| eyre!("Fetch.Cave response lacked cave.installInfo.installFolder"))
}

fn maybe_print_butler_notification(message: &Value) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    if method.contains("Progress") || method.contains("Task") {
        eprintln!("butlerd: {method}");
    }
}

fn json_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| eyre!("JSON value lacked string field `{key}`"))
}

fn json_u64(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| eyre!("JSON value lacked numeric field `{key}`"))
}

fn relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    path.strip_prefix(base)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

fn xdg_data_home(home: &Path) -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local").join("share"))
}

fn xdg_state_home(home: &Path) -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local").join("state"))
}

fn xdg_cache_home(home: &Path) -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".cache"))
}

fn itch_state_home(home: &Path) -> PathBuf {
    xdg_state_home(home).join("gamectl").join("itch")
}

fn itch_cache_home(home: &Path) -> PathBuf {
    xdg_cache_home(home).join("gamectl").join("itch")
}

fn itch_butler_bin(home: &Path) -> PathBuf {
    xdg_data_home(home)
        .join("gamectl")
        .join("itch")
        .join("butler")
        .join("bin")
        .join("butler")
}

fn itch_butler_version(home: &Path) -> PathBuf {
    xdg_data_home(home)
        .join("gamectl")
        .join("itch")
        .join("butler")
        .join("VERSION")
}

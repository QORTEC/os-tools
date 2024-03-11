// SPDX-FileCopyrightText: Copyright © 2020-2024 Serpent OS Developers
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    fs::{self, create_dir_all},
    io,
    os::{fd::RawFd, unix::fs::symlink},
    path::{Path, PathBuf},
    time::Duration,
};

use futures::{stream, StreamExt, TryStreamExt};
use itertools::Itertools;
use nix::{
    errno::Errno,
    fcntl::{self, OFlag},
    libc::{syscall, SYS_renameat2, AT_FDCWD, RENAME_EXCHANGE},
    sys::stat::{fchmodat, mkdirat, Mode},
    unistd::{close, linkat, mkdir, symlinkat},
};
use stone::{payload::layout, read::PayloadKind};
use thiserror::Error;
use tui::{MultiProgress, ProgressBar, ProgressStyle, Styled};
use vfs::tree::{builder::TreeBuilder, BlitFile, Element};

use self::install::install;
use self::prune::prune;
use crate::{
    db, environment, installation, package,
    registry::plugin::{self, Plugin},
    repository, runtime,
    state::{self, Selection},
    Installation, Package, Registry, State,
};

pub mod cache;
pub mod install;
mod postblit;
pub mod prune;

/// A Client is a connection to the underlying package management systems
pub struct Client {
    pub name: String,
    /// Root that we operate on
    pub installation: Installation,
    pub registry: Registry,

    pub install_db: db::meta::Database,
    pub state_db: db::state::Database,
    pub layout_db: db::layout::Database,

    config: config::Manager,
    repositories: repository::Manager,
    scope: Scope,
}

impl Client {
    /// Construct a new Client
    pub fn new(client_name: impl ToString, installation: Installation) -> Result<Client, Error> {
        Self::build(client_name, installation, None)
    }

    /// Construct a new Client with explicit repositories
    pub fn with_explicit_repositories(
        client_name: impl ToString,
        installation: Installation,
        repositories: repository::Map,
    ) -> Result<Client, Error> {
        Self::build(client_name, installation, Some(repositories))
    }

    fn build(
        client_name: impl ToString,
        installation: Installation,
        repositories: Option<repository::Map>,
    ) -> Result<Client, Error> {
        let name = client_name.to_string();
        let config = config::Manager::system(&installation.root, "moss");
        let install_db = db::meta::Database::new(installation.db_path("install").to_str().unwrap_or_default())?;
        let state_db = db::state::Database::new(installation.db_path("state").to_str().unwrap_or_default())?;
        let layout_db = db::layout::Database::new(installation.db_path("layout").to_str().unwrap_or_default())?;

        let repositories = if let Some(repos) = repositories {
            repository::Manager::explicit(&name, repos, installation.clone())?
        } else {
            repository::Manager::system(config.clone(), installation.clone())?
        };

        let registry = build_registry(&installation, &repositories, &install_db, &state_db)?;

        Ok(Client {
            name,
            config,
            installation,
            repositories,
            registry,
            install_db,
            state_db,
            layout_db,
            scope: Scope::Stateful,
        })
    }

    pub fn is_ephemeral(&self) -> bool {
        matches!(self.scope, Scope::Ephemeral { .. })
    }

    pub fn install(&mut self, packages: &[&str], yes: bool) -> Result<install::Timing, install::Error> {
        install(self, packages, yes)
    }

    /// Transition to an ephemeral client that doesn't record state changes
    /// and blits to a different root.
    ///
    /// This is useful for installing a root to a container (i.e. Boulder) while
    /// using a shared cache.
    ///
    /// Returns an error if `blit_root` is the same as the installation root,
    /// since the system client should always be stateful.
    pub fn ephemeral(self, blit_root: impl Into<PathBuf>) -> Result<Self, Error> {
        let blit_root = blit_root.into();

        if blit_root.canonicalize()? == self.installation.root.canonicalize()? {
            return Err(Error::EphemeralInstallationRoot);
        }

        Ok(Self {
            scope: Scope::Ephemeral { blit_root },
            ..self
        })
    }

    /// Ensures all repositories have been initialized by ensuring their stone indexes
    /// are downloaded and added to the meta db
    pub async fn ensure_repos_initialized(&mut self) -> Result<usize, Error> {
        let num_initialized = self.repositories.ensure_all_initialized().await?;
        self.registry = build_registry(&self.installation, &self.repositories, &self.install_db, &self.state_db)?;
        Ok(num_initialized)
    }

    /// Reload all configured repositories and refreshes their index file, then update
    /// registry with all active repositories.
    pub async fn refresh_repositories(&mut self) -> Result<(), Error> {
        // Reload manager if not explicit to pickup config changes
        // then refresh indexes
        if !self.repositories.is_explicit() {
            self.repositories = repository::Manager::system(self.config.clone(), self.installation.clone())?
        };
        self.repositories.refresh_all().await?;

        // Rebuild registry
        self.registry = build_registry(&self.installation, &self.repositories, &self.install_db, &self.state_db)?;

        Ok(())
    }

    /// Prune states with the provided [`prune::Strategy`]
    pub fn prune(&self, strategy: prune::Strategy, yes: bool) -> Result<(), Error> {
        if self.scope.is_ephemeral() {
            return Err(Error::EphemeralProhibitedOperation);
        }

        prune(
            strategy,
            &self.state_db,
            &self.install_db,
            &self.layout_db,
            &self.installation,
            yes,
        )?;
        Ok(())
    }

    /// Resolves the provided id's with the underlying registry, returning
    /// the first [`Package`] for each id. Packages are sorted by name
    /// and deduped before returning.
    pub fn resolve_packages<'a>(
        &self,
        packages: impl IntoIterator<Item = &'a package::Id>,
    ) -> Result<Vec<Package>, Error> {
        let mut metadata = packages
            .into_iter()
            .map(|id| self.registry.by_id(id).next().ok_or(Error::MissingMetadata(id.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        metadata.sort_by_key(|p| p.meta.name.to_string());
        metadata.dedup_by_key(|p| p.meta.name.to_string());
        Ok(metadata)
    }

    /// Activates the provided state and runs system triggers
    /// once applied. The current state gets archived.
    ///
    /// Returns the old state that was archived
    pub fn activate_state(&self, id: state::Id) -> Result<state::Id, Error> {
        // Fetch the new state
        let new = self.state_db.get(id).map_err(|_| Error::StateDoesntExist(id))?;

        // Get old (current) state
        let Some(old) = self.installation.active_state else {
            return Err(Error::NoActiveState);
        };

        if new.id == old {
            return Err(Error::StateAlreadyActive(id));
        }

        let staging_dir = self.installation.staging_dir();

        // Ensure staging dir exists
        if !staging_dir.exists() {
            fs::create_dir(&staging_dir)?;
        }

        // Move new (archived) state to staging
        fs::rename(self.installation.root_path(new.id.to_string()), &staging_dir)?;

        // Promote staging
        self.promote_staging()?;

        // Archive old state
        self.archive_state(old)?;

        // Build VFS from new state selections
        // to build triggers from
        let fstree = self.vfs(new.selections.iter().map(|selection| &selection.package))?;

        // Run system triggers
        let sys_triggers =
            postblit::triggers(postblit::TriggerScope::System(&self.installation, &self.scope), &fstree)?;
        for trigger in sys_triggers {
            trigger.execute()?;
        }

        Ok(old)
    }

    /// Create a new recorded state from the provided packages
    /// provided packages and write that state ID to the installation
    /// Then blit the filesystem, promote it, finally archiving the active ID
    ///
    /// Returns `None` if the client is ephemeral
    pub fn new_state(&self, selections: &[Selection], summary: impl ToString) -> Result<Option<State>, Error> {
        let old_state = self.installation.active_state;

        let fstree = self.blit_root(selections.iter().map(|s| &s.package))?;

        match &self.scope {
            Scope::Stateful => {
                // Add to db
                let state = self.state_db.add(selections, Some(&summary.to_string()), None)?;

                // Write state id
                {
                    let usr = self.installation.staging_path("usr");
                    fs::create_dir_all(&usr)?;
                    let state_path = usr.join(".stateID");
                    fs::write(state_path, state.id.to_string())?;
                }

                record_os_release(&self.installation.staging_dir(), Some(state.id))?;

                // Run all of the transaction triggers
                let triggers = postblit::triggers(
                    postblit::TriggerScope::Transaction(&self.installation, &self.scope),
                    &fstree,
                )?;
                create_root_links(&self.installation.isolation_dir())?;
                for trigger in triggers {
                    trigger.execute()?;
                }
                // Staging is only used with [`Scope::Stateful`]
                self.promote_staging()?;

                // Now we got it staged, we need working rootfs
                create_root_links(&self.installation.root)?;

                if let Some(id) = old_state {
                    self.archive_state(id)?;
                }

                // At this point we're allowed to run system triggers
                let sys_triggers =
                    postblit::triggers(postblit::TriggerScope::System(&self.installation, &self.scope), &fstree)?;
                for trigger in sys_triggers {
                    trigger.execute()?;
                }

                Ok(Some(state))
            }
            Scope::Ephemeral { blit_root } => {
                record_os_release(blit_root, None)?;
                create_root_links(blit_root)?;
                create_root_links(&self.installation.isolation_dir())?;

                let etc = blit_root.join("etc");
                create_dir_all(etc)?;

                // ephemeral tx triggers
                let triggers = postblit::triggers(
                    postblit::TriggerScope::Transaction(&self.installation, &self.scope),
                    &fstree,
                )?;
                for trigger in triggers {
                    trigger.execute()?;
                }
                // ephemeral system triggers
                let sys_triggers =
                    postblit::triggers(postblit::TriggerScope::System(&self.installation, &self.scope), &fstree)?;
                for trigger in sys_triggers {
                    trigger.execute()?;
                }
                Ok(None)
            }
        }
    }

    /// Activate the given state
    fn promote_staging(&self) -> Result<(), Error> {
        if self.scope.is_ephemeral() {
            return Err(Error::EphemeralProhibitedOperation);
        }

        let usr_target = self.installation.root.join("usr");
        let usr_source = self.installation.staging_path("usr");

        // Create the target tree
        if !usr_target.try_exists()? {
            fs::create_dir_all(&usr_target)?;
        }

        // Now swap staging with live
        Self::atomic_swap(&usr_source, &usr_target)?;

        Ok(())
    }

    /// syscall based wrapper for renameat2 so we can support musl libc which
    /// unfortunately does not expose the API.
    /// largely modelled on existing renameat2 API in nix crae
    fn atomic_swap<A: ?Sized + nix::NixPath, B: ?Sized + nix::NixPath>(old_path: &A, new_path: &B) -> nix::Result<()> {
        let result = old_path.with_nix_path(|old| {
            new_path.with_nix_path(|new| unsafe {
                syscall(
                    SYS_renameat2,
                    AT_FDCWD,
                    old.as_ptr(),
                    AT_FDCWD,
                    new.as_ptr(),
                    RENAME_EXCHANGE,
                )
            })
        })?? as i32;
        Errno::result(result).map(drop)
    }

    /// Archive old states into their respective tree
    fn archive_state(&self, id: state::Id) -> Result<(), Error> {
        if self.scope.is_ephemeral() {
            return Err(Error::EphemeralProhibitedOperation);
        }

        // After promotion, the old active /usr is now in staging/usr
        let usr_target = self.installation.root_path(id.to_string()).join("usr");
        let usr_source = self.installation.staging_path("usr");
        if let Some(parent) = usr_target.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        // hot swap the staging/usr into the root/$id/usr
        fs::rename(usr_source, &usr_target)?;
        Ok(())
    }

    /// Download & unpack the provided packages. Packages already cached will be validated & skipped.
    pub async fn cache_packages(&self, packages: &[&Package]) -> Result<(), Error> {
        // Setup progress bar
        let multi_progress = MultiProgress::new();

        // Add bar to track total package counts
        let total_progress = multi_progress.add(
            ProgressBar::new(packages.len() as u64).with_style(
                ProgressStyle::with_template("\n|{bar:20.cyan/blue}| {pos}/{len}")
                    .unwrap()
                    .progress_chars("■≡=- "),
            ),
        );
        total_progress.tick();

        let unpacking_in_progress = cache::UnpackingInProgress::default();

        // Download and unpack each package
        stream::iter(packages.iter().map(|package| async {
            // Setup the progress bar and set as downloading
            let progress_bar = multi_progress.insert_before(
                &total_progress,
                ProgressBar::new(package.meta.download_size.unwrap_or_default())
                    .with_message(format!(
                        "{} {}",
                        "Downloading".blue(),
                        package.meta.name.to_string().bold(),
                    ))
                    .with_style(
                        ProgressStyle::with_template(
                            " {spinner} |{percent:>3}%| {wide_msg} {binary_bytes_per_sec:>.dim} ",
                        )
                        .unwrap()
                        .tick_chars("--=≡■≡=--"),
                    ),
            );
            progress_bar.enable_steady_tick(Duration::from_millis(150));

            // Download and update progress
            let download = cache::fetch(&package.meta, &self.installation, |progress| {
                progress_bar.inc(progress.delta);
            })
            .await?;
            let is_cached = download.was_cached;

            // Move rest of blocking code to threadpool

            let multi_progress = multi_progress.clone();
            let total_progress = total_progress.clone();
            let unpacking_in_progress = unpacking_in_progress.clone();
            let layout_db = self.layout_db.clone();
            let install_db = self.install_db.clone();
            let package = (*package).clone();

            runtime::unblock(move || {
                let package_name = package.meta.name.to_string();

                // Set progress to unpacking
                progress_bar.set_message(format!("{} {}", "Unpacking".yellow(), package_name.clone().bold(),));
                progress_bar.set_length(1000);
                progress_bar.set_position(0);

                // Unpack and update progress
                let unpacked = download.unpack(unpacking_in_progress.clone(), {
                    let progress_bar = progress_bar.clone();

                    move |progress| {
                        progress_bar.set_position((progress.pct() * 1000.0) as u64);
                    }
                })?;

                // Merge layoutdb
                progress_bar.set_message(format!("{} {}", "Store layout".white(), package_name.clone().bold()));
                // Remove old layout entries for package
                layout_db.remove(&package.id)?;
                // Add new entries in batches of 1k
                for chunk in progress_bar.wrap_iter(
                    unpacked
                        .payloads
                        .iter()
                        .find_map(PayloadKind::layout)
                        .map(|p| &p.body)
                        .ok_or(Error::CorruptedPackage)?
                        .chunks(environment::DB_BATCH_SIZE),
                ) {
                    let entries = chunk.iter().map(|i| (package.id.clone(), i.clone())).collect_vec();
                    layout_db.batch_add(entries)?;
                }

                // Consume the package in the metadb
                install_db.add(package.id.clone(), package.meta.clone())?;

                // Remove this progress bar
                progress_bar.finish();
                multi_progress.remove(&progress_bar);

                let cached_tag = is_cached
                    .then_some(format!("{}", " (cached)".dim()))
                    .unwrap_or_default();

                // Write installed line
                multi_progress.println(format!(
                    "{} {}{}",
                    "Installed".green(),
                    package_name.clone().bold(),
                    cached_tag,
                ))?;

                // Inc total progress by 1
                total_progress.inc(1);

                Ok(()) as Result<(), Error>
            })
            .await
        }))
        // Use max network concurrency since we download files here
        .buffer_unordered(environment::MAX_NETWORK_CONCURRENCY)
        .try_collect()
        .await?;

        // Remove progress
        multi_progress.clear()?;

        Ok(())
    }

    /// Build a [`vfs::Tree`] for the provided packages
    pub fn vfs<'a>(
        &self,
        packages: impl IntoIterator<Item = &'a package::Id>,
    ) -> Result<vfs::Tree<PendingFile>, Error> {
        let mut tbuild = TreeBuilder::new();
        let layouts = self.layout_db.query(packages)?;
        for (id, layout) in layouts {
            tbuild.push(PendingFile { id: id.clone(), layout });
        }
        tbuild.bake();
        let tree = tbuild.tree()?;
        Ok(tree)
    }

    /// Blit the packages to a filesystem root
    fn blit_root<'a>(
        &self,
        packages: impl IntoIterator<Item = &'a package::Id>,
    ) -> Result<vfs::tree::Tree<PendingFile>, Error> {
        let progress = ProgressBar::new(1).with_style(
            ProgressStyle::with_template("\n|{bar:20.red/blue}| {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("■≡=- "),
        );
        progress.set_message("Blitting filesystem");
        progress.enable_steady_tick(Duration::from_millis(150));
        progress.tick();

        let tree = self.vfs(packages)?;

        progress.set_length(tree.len());
        progress.set_position(0_u64);

        let cache_dir = self.installation.assets_path("v2");
        let cache_fd = fcntl::open(&cache_dir, OFlag::O_DIRECTORY | OFlag::O_RDONLY, Mode::empty())?;

        let blit_target = match &self.scope {
            Scope::Stateful => self.installation.staging_dir(),
            Scope::Ephemeral { blit_root } => blit_root.to_owned(),
        };

        // undirt.
        fs::remove_dir_all(&blit_target)?;

        if let Some(root) = tree.structured() {
            let _ = mkdir(&blit_target, Mode::from_bits_truncate(0o755));
            let root_dir = fcntl::open(&blit_target, OFlag::O_DIRECTORY | OFlag::O_RDONLY, Mode::empty())?;

            if let Element::Directory(_, _, children) = root {
                for child in children {
                    self.blit_element(root_dir, cache_fd, child, &progress)?;
                }
            }

            close(root_dir)?;
        }

        Ok(tree)
    }

    /// blit an element to the disk.
    fn blit_element(
        &self,
        parent: RawFd,
        cache: RawFd,
        element: Element<PendingFile>,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        progress.inc(1);
        match element {
            Element::Directory(name, item, children) => {
                // Construct within the parent
                self.blit_element_item(parent, cache, &name, item)?;

                // open the new dir
                let newdir = fcntl::openat(
                    parent,
                    name.as_str(),
                    OFlag::O_RDONLY | OFlag::O_DIRECTORY,
                    Mode::empty(),
                )?;
                for child in children.into_iter() {
                    self.blit_element(newdir, cache, child, progress)?;
                }
                close(newdir)?;
                Ok(())
            }
            Element::Child(name, item) => {
                self.blit_element_item(parent, cache, &name, item)?;
                Ok(())
            }
        }
    }

    /// Process the raw layout entry.
    fn blit_element_item(&self, parent: RawFd, cache: RawFd, subpath: &str, item: PendingFile) -> Result<(), Error> {
        match item.layout.entry {
            layout::Entry::Regular(id, _) => {
                let hash = format!("{:02x}", id);
                let directory = if hash.len() >= 10 {
                    PathBuf::from(&hash[..2]).join(&hash[2..4]).join(&hash[4..6])
                } else {
                    "".into()
                };

                // Link relative from cache to target
                let fp = directory.join(hash);
                linkat(
                    Some(cache),
                    fp.to_str().unwrap(),
                    Some(parent),
                    subpath,
                    nix::unistd::LinkatFlags::NoSymlinkFollow,
                )?;

                // Fix permissions
                fchmodat(
                    Some(parent),
                    subpath,
                    Mode::from_bits_truncate(item.layout.mode),
                    nix::sys::stat::FchmodatFlags::NoFollowSymlink,
                )?;
            }
            layout::Entry::Symlink(source, _) => {
                symlinkat(source.as_str(), Some(parent), subpath)?;
            }
            layout::Entry::Directory(_) => {
                mkdirat(parent, subpath, Mode::from_bits_truncate(item.layout.mode))?;
            }

            // unimplemented
            layout::Entry::CharacterDevice(_) => todo!(),
            layout::Entry::BlockDevice(_) => todo!(),
            layout::Entry::Fifo(_) => todo!(),
            layout::Entry::Socket(_) => todo!(),
        };

        Ok(())
    }
}

/// Add root symlinks & os-release file
fn create_root_links(root: &Path) -> Result<(), io::Error> {
    let links = vec![
        ("usr/sbin", "sbin"),
        ("usr/bin", "bin"),
        ("usr/lib", "lib"),
        ("usr/lib", "lib64"),
        ("usr/lib32", "lib32"),
    ];

    'linker: for (source, target) in links.into_iter() {
        let final_target = root.join(target);
        let staging_target = root.join(format!("{target}.next"));

        if staging_target.exists() {
            fs::remove_file(&staging_target)?;
        }

        if final_target.exists() && final_target.is_symlink() && final_target.read_link()?.to_string_lossy() == source {
            continue 'linker;
        }
        symlink(source, &staging_target)?;
        fs::rename(staging_target, final_target)?;
    }

    Ok(())
}

/// Record the operating system release info
fn record_os_release(root: &Path, state_id: Option<state::Id>) -> Result<(), Error> {
    let os_release = format!(
        r#"NAME="Serpent OS"
VERSION="{version}"
ID="serpentos"
VERSION_CODENAME={version}
VERSION_ID="{version}"
PRETTY_NAME="Serpent OS {version} (fstx #{tx})"
ANSI_COLOR="1;35"
HOME_URL="https://serpentos.com"
BUG_REPORT_URL="https://github.com/serpent-os""#,
        version = environment::VERSION,
        // TODO: Better id for ephemeral transactions
        tx = state_id.unwrap_or_default()
    );

    // It's possible this doesn't exist if
    // we remove all packages (=
    let dir = root.join("usr").join("lib");
    if !dir.exists() {
        fs::create_dir(&dir)?;
    }

    fs::write(dir.join("os-release"), os_release)?;

    Ok(())
}

#[derive(Clone, Debug)]
enum Scope {
    Stateful,
    Ephemeral { blit_root: PathBuf },
}

impl Scope {
    fn is_ephemeral(&self) -> bool {
        matches!(self, Self::Ephemeral { .. })
    }
}

/// A pending file for blitting
#[derive(Debug, Clone)]
pub struct PendingFile {
    pub id: package::Id,
    pub layout: layout::Layout,
}

impl BlitFile for PendingFile {
    /// Match internal kind to minimalist vfs kind
    fn kind(&self) -> vfs::tree::Kind {
        match &self.layout.entry {
            layout::Entry::Symlink(source, _) => vfs::tree::Kind::Symlink(source.clone()),
            layout::Entry::Directory(_) => vfs::tree::Kind::Directory,
            _ => vfs::tree::Kind::Regular,
        }
    }

    /// Return ID for conflict
    fn id(&self) -> String {
        self.id.clone().into()
    }

    /// Resolve the target path, including the missing `/usr` prefix
    fn path(&self) -> String {
        let result = match &self.layout.entry {
            layout::Entry::Regular(_, target) => target.clone(),
            layout::Entry::Symlink(_, target) => target.clone(),
            layout::Entry::Directory(target) => target.clone(),
            layout::Entry::CharacterDevice(target) => target.clone(),
            layout::Entry::BlockDevice(target) => target.clone(),
            layout::Entry::Fifo(target) => target.clone(),
            layout::Entry::Socket(target) => target.clone(),
        };

        vfs::path::join("/usr", &result)
    }

    /// Clone the node to a reparented path, for symlink resolution
    fn cloned_to(&self, path: String) -> Self {
        let mut new = self.clone();
        new.layout.entry = match &self.layout.entry {
            layout::Entry::Regular(source, _) => layout::Entry::Regular(*source, path),
            layout::Entry::Symlink(source, _) => layout::Entry::Symlink(source.clone(), path),
            layout::Entry::Directory(_) => layout::Entry::Directory(path),
            layout::Entry::CharacterDevice(_) => layout::Entry::CharacterDevice(path),
            layout::Entry::BlockDevice(_) => layout::Entry::BlockDevice(path),
            layout::Entry::Fifo(_) => layout::Entry::Fifo(path),
            layout::Entry::Socket(_) => layout::Entry::Socket(path),
        };
        new
    }
}

impl From<String> for PendingFile {
    fn from(value: String) -> Self {
        PendingFile {
            id: Default::default(),
            layout: layout::Layout {
                uid: 0,
                gid: 0,
                mode: 0o755,
                tag: 0,
                entry: layout::Entry::Directory(value),
            },
        }
    }
}

impl ToString for PendingFile {
    fn to_string(&self) -> String {
        self.path()
    }
}

fn build_registry(
    installation: &Installation,
    repositories: &repository::Manager,
    installdb: &db::meta::Database,
    statedb: &db::state::Database,
) -> Result<Registry, Error> {
    let state = match installation.active_state {
        Some(id) => Some(statedb.get(id)?),
        None => None,
    };

    let mut registry = Registry::default();

    registry.add_plugin(Plugin::Cobble(plugin::Cobble::default()));
    registry.add_plugin(Plugin::Active(plugin::Active::new(state, installdb.clone())));

    for repo in repositories.active() {
        registry.add_plugin(Plugin::Repository(plugin::Repository::new(repo)));
    }

    Ok(registry)
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("root must have an active state")]
    NoActiveState,
    #[error("Corrupted package")]
    CorruptedPackage,
    #[error("state {0} already active")]
    StateAlreadyActive(state::Id),
    #[error("state {0} doesn't exist")]
    StateDoesntExist(state::Id),
    #[error("No metadata found for package {0:?}")]
    MissingMetadata(package::Id),
    #[error("Ephemeral client not allowed on installation root")]
    EphemeralInstallationRoot,
    #[error("Operation not allowed with ephemeral client")]
    EphemeralProhibitedOperation,
    #[error("installation")]
    Installation(#[from] installation::Error),
    #[error("cache")]
    Cache(#[from] cache::Error),
    #[error("repository manager")]
    Repository(#[from] repository::manager::Error),
    #[error("db")]
    Meta(#[from] db::Error),
    #[error("prune")]
    Prune(#[from] prune::Error),
    #[error("io")]
    Io(#[from] io::Error),
    #[error("filesystem")]
    Filesystem(#[from] vfs::tree::Error),
    #[error("blit")]
    Blit(#[from] Errno),
    #[error("postblit")]
    PostBlit(#[from] postblit::Error),
}

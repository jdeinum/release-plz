use crate::cargo::run_cargo;
use crate::update_request::UpdateRequest;
use crate::updater::Updater;
use crate::{
    PackagesUpdate, Project,
    changelog_parser::{self, ChangelogRelease},
    copy_dir::copy_dir,
    fs_utils::{Utf8TempDir, strip_prefix},
    package_path::manifest_dir,
    registry_packages::{self},
    semver_check::SemverCheck,
};
use anyhow::Context;
use cargo_metadata::TargetKind;
use cargo_metadata::{
    Metadata, Package,
    camino::{Utf8Path, Utf8PathBuf},
    semver::Version,
};
use chrono::NaiveDate;
use git_cmd::Repo;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use toml_edit::TableLike;
use tracing::{debug, info, instrument, trace, warn};

// Used to indicate that this is a dummy commit with no corresponding ID available.
// It should be at least 7 characters long to avoid a panic in git-cliff
// (Git-cliff assumes it's a valid commit ID).
pub(crate) const NO_COMMIT_ID: &str = "0000000";

#[derive(Debug, Clone)]
pub struct ReleaseMetadata {
    /// Template for the git tag created by release-plz.
    pub tag_name_template: Option<String>,
    /// Template for the git release name created by release-plz.
    pub release_name_template: Option<String>,
}

pub trait ReleaseMetadataBuilder {
    fn get_release_metadata(&self, package_name: &str) -> Option<ReleaseMetadata>;
}

#[derive(Debug, Clone, Default)]
pub struct ChangelogRequest {
    /// When the new release is published. If unspecified, current date is used.
    pub release_date: Option<NaiveDate>,
    pub changelog_config: Option<git_cliff_core::config::Config>,
}

impl ReleaseMetadataBuilder for UpdateRequest {
    fn get_release_metadata(&self, package_name: &str) -> Option<ReleaseMetadata> {
        let config = self.get_package_config(package_name);
        config.generic.release.then(|| ReleaseMetadata {
            tag_name_template: config.generic.tag_name_template.clone(),
            release_name_template: None,
        })
    }
}

/// Determine next version of packages
/// Any packages that will be updated will be returned, alongside whether we update the workspace
/// The temp repository is the
#[instrument(skip_all)]
pub async fn next_versions(input: &UpdateRequest) -> anyhow::Result<(PackagesUpdate, Repo)> {
    info!("determining next version");
    let overrides = input.packages_config().overridden_packages();

    let local_project = Project::new(
        input.local_manifest(),
        input.single_package(),
        &overrides,
        input.cargo_metadata(),
        input,
    )?;
    let updater = Updater {
        project: &local_project,
        req: input,
    };

    let release_packages = if input.git_only().is_some()
    // If we are only using git-only mode, we get release information from git tags rather than
    // using a registry of some kind. The steps are roughly as follows:
    //
    // 1. Get the path to the local package with the unreleased changes (i.e working copy)
    // 2. Create a git worktree somewhere in /tmp at the current commit (HEAD), we'll call it
    //    WT_CURRENT
    // 3. Inspect the commit log in WT_CURRENT until we find a commit that satisfies our tag
    //    release regex, call it commit X
    // 4. Create a second git worktree in /tmp , and checkout to commit X, we'll call it
    //    WT_LATEST_RELEASE
    // 5. Run cargo package in WT_LATEST_RELEASE to create the crate tarball
    // 6. Extract it somewhere in /tmp, probably just inside of the WT_LATEST_RELEASE, call it
    //    LATEST_PACKAGE
    // 7. Get the path to LATEST_PACKAGE, and return it
    //
    //
    // NOTE: We may actually have to run `cargo package` in the unreleased version worktree (i.e our
    // current copy) to ensure that we are comparing properly. I assume that this is already
    // handled somewhere because we can run this command in a standard rust project (not just
    // within a cargo package dir)
    // TODO: Verify this ^
    {
        debug!("Git only mode enabled, using git tags for release info");

        // create a repo at the unreleased package worktree
        // TODO: This was already done in the parent. Do we really have to do this again?
        let unreleased_package_worktree: Repo = Repo::new(
            input
                .local_manifest_dir()
                .context("get unreleased package dir parent")?,
        )
        .context("open git repo for unreleased worktree")?;

        // now we scan the commit history to find the commits that satisfy our regex
        // TODO: Probably don't use unwrap here, or atleast make sure we've checked it prior to
        // this invocation
        let tag_with_commit: Option<(String, String)> = unreleased_package_worktree
            .get_tags_with_commits()
            .context("get tags with commits")?
            .iter()
            .filter(|x| input.git_only_release_regex().unwrap().is_match(&x.0))
            .next()
            .cloned();

        let mut local_release_path: Option<String> = None;

        if let Some((tag, commit)) = tag_with_commit {
            info!(
                "tag {} matched release regex {:?} with commit {commit}",
                tag,
                input.git_only_release_regex()
            );

            // create a new worktree at the specfied commit
            let latest_release_worktree_path = "/tmp/latest_release";
            debug!("latest release worktree located at {latest_release_worktree_path}");
            delete_existing_worktree(latest_release_worktree_path.into())
                .await
                .with_context(|| "delete existing worktree for unreleased_package_worktree_path")?;

            // create a worktree for the latest released version
            unreleased_package_worktree
                .add_worktree(&latest_release_worktree_path, Some(&commit))
                .context("create git worktree for latest release")?;

            // run cargo package to produce our package
            run_cargo(latest_release_worktree_path.into(), &["package"])
                .context("create cargo package")?;

            let latest_release_package_path =
                format!("{latest_release_worktree_path}/target/package");

            // at this point we need to update the latest_release path for all of the packages
            // we do that by finding the name of the published directory from the input, and
            // replacing the base path to that of the {latest_release_worktree/target/package} so
            // that all of the local manifests point into our packaged versions in /tmp
            let mut local_project_copy = local_project.clone();
            local_project_copy
                .workspace_packages_mut()
                .iter_mut()
                .for_each(|x| {
                    x.manifest_path =
                        format!("{}/{}-{}", latest_release_package_path, x.name, x.version).into();
                });
            info!("local_project_copy: {local_project_copy:?}");

            // now we need to initialize a git repo in each of the repo packages
            // TODO

            // create a git repository in the packaged version to make sure comparisons are equivilent
            info!(
                "initializing git repo in the latest release package at {latest_release_package_path}"
            );
            // Repo::init_simple(&latest_release_package_path)
            //     .context("create git repo for package dir")?;

            local_release_path = Some(format!("{latest_release_worktree_path}/Cargo.toml"));
        }
        // NOTE: If no tags match the regex, I believe we default to using the registry for the
        // first release, which will throw and error, and instead say the
        else {
            warn!("No tag found that matches regex, defaulting to current implementation");
        }

        // get the path to the new packaged version, the updated path is at
        // /tmp/package_name/target/package/package_name-0.1.0
        // so either we need to use the specific version from metadata or we need to find it
        // dynamically. Once thing I am, unsure about is how this plays out with workspaces. There
        // would be a directory for each of the packages in the workspace, but they aren't in a
        // real workspace from cargos perspective.

        let local_release_path_utf8: Option<Utf8PathBuf> =
            local_release_path.map(|s| Utf8PathBuf::from(s));

        // If the local manifest path is populated (i.e from CLI args) then we always prefer that,
        // otherwise we'll create a local copy of the released package and provide the path to its
        // Cargo.toml.
        let local_release_path = match input.registry_manifest() {
            Some(s) => Some(s.into()),
            None => local_release_path_utf8,
        };

        debug!("Using local package for current latest release info at {local_release_path:?}");

        // we can just pass on our local copy of the package, the rest of the system doesn't even
        // have to know
        registry_packages::get_registry_packages(
            local_release_path.as_deref(),
            &local_project.publishable_packages(),
            input.registry(),
        )
    }
    // Retrieve the latest published version of the packages.
    // Release-plz will compare the registry packages with the local packages,
    // to determine the new commits.
    else {
        debug!("Using registry packages for release info!");
        registry_packages::get_registry_packages(
            input.registry_manifest(),
            &local_project.publishable_packages(),
            input.registry(),
        )
    }?;

    // The repository that contains the unpublished changes
    let unreleased_worktree = Repo::new(
        input
            .local_manifest()
            .parent()
            .context("get local manifest parent")?,
    )
    .context("create unreleased worktree")?;

    info!("unreleased_worktree: {unreleased_worktree:?}");

    // If our unreleased_repo contains changes that aren't checked in to git and we don't allow
    // dirty repos, we error out. Otherwise, we just stash the changes. Note that because these
    // changes aren't commited, no version change would occur because of them.
    let repo_is_clean_result = unreleased_worktree.is_clean();
    if !input.allow_dirty() {
        repo_is_clean_result?;
    } else if repo_is_clean_result.is_err() {
        // Stash uncommitted changes so we can freely check out other commits.
        // This function is ran inside a temporary repository, so this has no
        // effects on the original repository of the user.
        unreleased_worktree.git(&[
            "stash",
            "push",
            "--include-untracked",
            "-m",
            "uncommitted changes stashed by release-plz",
        ])?;
    }

    let packages_to_update = updater
        .packages_to_update(
            &release_packages,
            &unreleased_worktree,
            input.local_manifest(),
        )
        .await?;
    Ok((packages_to_update, unreleased_worktree))
}

pub async fn delete_existing_worktree(path: &Utf8Path) -> anyhow::Result<()> {
    // We still have to check if this worktree exists already. It shouldn't, but because NTP
    // could in theory step the clock back, we may as well check.
    let f = tokio::fs::File::open(&path).await;

    // doesn't exist, great
    if f.as_ref().is_err_and(|x| x.kind() == ErrorKind::NotFound) {
        debug!("{path} not found, nothing to delete");
    }
    // does exist, delete it
    else {
        match f?.metadata().await {
            Ok(m) if m.is_dir() => {
                warn!(
                    "{path} already exists, are you a time traveller? Just kidding, but we are deleting it for consistency"
                );
                tokio::fs::remove_dir_all(&path)
                    .await
                    .context("delete existing unreleased package")?;
            }

            // either file or symlink, i don't think we care though? we'll just remove it
            Ok(_) => {
                warn!("{path} already exists as a file, deleting it");
                tokio::fs::remove_file(&path)
                    .await
                    .context("delete existing unreleased package file")?;
            }

            // if its not found, then great!
            Err(e) => return Err(anyhow::anyhow!(e)),
        };
    }

    Ok(())
}

pub fn root_repo_path(local_manifest: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let manifest_dir = manifest_dir(local_manifest)?;
    root_repo_path_from_manifest_dir(manifest_dir)
}

pub fn root_repo_path_from_manifest_dir(manifest_dir: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let root = git_cmd::git_in_dir(manifest_dir, &["rev-parse", "--show-toplevel"])?;
    Ok(Utf8PathBuf::from(root))
}

pub fn new_manifest_dir_path(
    old_project_root: &Utf8Path,
    old_manifest_dir: &Utf8Path,
    new_project_root: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    let parent_root = old_project_root.parent().unwrap_or(old_project_root);
    let relative_manifest_dir = strip_prefix(old_manifest_dir, parent_root)
        .context("cannot strip prefix for manifest dir")?;
    Ok(new_project_root.join(relative_manifest_dir))
}

#[derive(Debug, Clone)]
pub struct UpdateResult {
    /// Next version of the package.
    pub version: Version,
    /// New changelog.
    pub changelog: Option<String>,
    pub semver_check: SemverCheck,
    pub new_changelog_entry: Option<String>,
}

impl UpdateResult {
    pub fn last_changes(&self) -> anyhow::Result<Option<ChangelogRelease>> {
        match &self.changelog {
            Some(c) => changelog_parser::last_release_from_str(c),
            None => Ok(None),
        }
    }
}

pub fn workspace_packages(metadata: &Metadata) -> anyhow::Result<Vec<Package>> {
    cargo_utils::workspace_members(metadata).map(|members| members.collect())
}

pub fn publishable_packages_from_manifest(
    manifest: impl AsRef<Utf8Path>,
) -> anyhow::Result<Vec<Package>> {
    let metadata = cargo_utils::get_manifest_metadata(manifest.as_ref())?;
    cargo_utils::workspace_members(&metadata)
        .map(|members| members.filter(|p| p.is_publishable()).collect())
}

pub trait Publishable {
    fn is_publishable(&self) -> bool;
}

impl Publishable for Package {
    /// Return true if the package can be published to at least one register (e.g. crates.io).
    fn is_publishable(&self) -> bool {
        let res = if let Some(publish) = &self.publish {
            // `publish.is_empty()` is:
            // - true: when `publish` in Cargo.toml is `[]` or `false`.
            // - false: when the package can be published only to certain registries.
            //          E.g. when `publish` in Cargo.toml is `["my-reg"]` or `true`.
            !publish.is_empty()
        } else {
            // If it's not an example, the package can be published anywhere
            !is_example_package(self)
        };
        trace!("package {} is publishable: {res}", self.name);
        res
    }
}

fn is_example_package(package: &Package) -> bool {
    package
        .targets
        .iter()
        .all(|t| t.kind == [TargetKind::Example])
}

pub fn copy_to_temp_dir(target: &Utf8Path) -> anyhow::Result<Utf8TempDir> {
    let tmp_dir = Utf8TempDir::new().context("cannot create temporary directory")?;
    copy_dir(target, tmp_dir.path())
        .with_context(|| format!("cannot copy directory {target:?} to {tmp_dir:?}"))?;
    Ok(tmp_dir)
}

/// Check if `dependency` (contained in the Cargo.toml at `dependency_package_dir`) refers
/// to the package at `package_dir`.
/// I.e. if the absolute path of the dependency is the same as the absolute path of the package.
pub(crate) fn is_dependency_referred_to_package(
    dependency: &dyn TableLike,
    package_dir: &Utf8Path,
    dependency_package_dir: &Utf8Path,
) -> bool {
    canonicalized_path(dependency, package_dir)
        .is_some_and(|dep_path| dep_path == dependency_package_dir)
}

/// Dependencies are expressed as relative paths in the Cargo.toml file.
/// This function returns the absolute path of the dependency.
///
/// ## Args
///
/// - `package_dir`: directory containing the Cargo.toml where the dependency is listed
/// - `dependency`: entry of the Cargo.toml
fn canonicalized_path(dependency: &dyn TableLike, package_dir: &Utf8Path) -> Option<PathBuf> {
    dependency
        .get("path")
        .and_then(|i| i.as_str())
        .and_then(|relpath| dunce::canonicalize(package_dir.join(relpath)).ok())
}

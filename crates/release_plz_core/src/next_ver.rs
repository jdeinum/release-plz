use crate::cargo::run_cargo;
use crate::command::git::{CustomRepo, CustomWorkTree};
use crate::registry_packages::{PackagesCollection, RegistryPackage};
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
use cargo_metadata::{
    Metadata, Package,
    camino::{Utf8Path, Utf8PathBuf},
    semver::Version,
};
use cargo_metadata::{MetadataCommand, PackageName, TargetKind};
use chrono::NaiveDate;
use git_cmd::Repo;
use git2::Oid;
use std::collections::{BTreeMap, HashMap};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
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

    // collect our packages
    let mut res: BTreeMap<String, RegistryPackage> = BTreeMap::new();

    // SAFETY: We need to prevent the worktrees from being dropped because they'll error out
    // otherwise
    let mut worktrees: Vec<CustomWorkTree> = Vec::new();

    let release_packages = if input.git_only().is_some() {
        debug!("Git only mode enabled, using git tags for release info");

        // create the repo we'll be spinning worktrees from, in this case, themselves worktrees!
        let unreleased_project_repo = CustomRepo::open(
            input
                .local_manifest_dir()
                .context("get local manifest dir")?,
        )
        .context("create unreleased repo for spinning worktrees")?;

        let release_regex = input.git_only_release_regex().unwrap();

        // create a commit for each package
        for package in input.cargo_metadata().workspace_packages() {
            // make a worktree for the package
            let worktree = unreleased_project_repo
                .temp_worktree(Some(package.name.to_string().as_str()), &package.name)
                .context("build worktree for package")?;

            // create repo at new worktree
            let mut repo = CustomRepo::open(worktree.path()).context("open repo for package")?;

            // get the tags
            let tags = repo.get_tags().context("get tags for package")?;

            // find the release tag
            let release_tag: Option<String> =
                tags.into_iter().find(|tag| release_regex.is_match(&tag));

            match release_tag {
                None => {
                    unimplemented!("Can't handle the case where release commit isn't defined yet")
                }

                Some(rt) => {
                    // get the commit associated with this tag
                    let release_commit =
                        repo.get_tag_commit(&rt).context("get release tag commit")?;

                    // checkout that commit in the worktree
                    repo.checkout_commit(&release_commit)
                        .context("checkout release commit for package")?;

                    // run cargo publish so we get the proper format
                    let c = std::process::Command::new("cargo")
                        .args(["package"])
                        .current_dir(worktree.path())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .context("run cargo package in worktree")?;
                    let _x = c.wait_with_output().context("wait for cargo package")?;

                    // create the package / registry Package
                    let rust_package = MetadataCommand::new()
                        .manifest_path(format!(
                            "{}/Cargo.toml",
                            worktree.path().to_str().unwrap_or_default()
                        ))
                        .exec()
                        .context("get cargo metadata")?;

                    let package_details = rust_package
                        .packages
                        .iter()
                        .filter(|x| x.name == package.name)
                        .next()
                        .unwrap();

                    let new_path = format!(
                        "{}/target/package/{}-{}",
                        worktree.path().to_str().unwrap_or_default(),
                        package_details.name,
                        package_details.version
                    );
                    info!("package for {} is at {}", package.name, new_path);

                    // SEE SAFETY NOTE ABOVE
                    worktrees.push(worktree);

                    // create the package
                    let single_package_meta = MetadataCommand::new()
                        .manifest_path(format!("{}/Cargo.toml", new_path))
                        .exec()
                        .context("get cargo metadata")?;

                    // get the package details
                    let single_package = single_package_meta
                        .workspace_packages()
                        .into_iter()
                        .find(|p| p.name == package.name)
                        .ok_or(anyhow::Error::msg("Couldn't find the package"))?
                        .clone();

                    // add it to the B Tree map
                    res.insert(
                        single_package.name.to_string(),
                        RegistryPackage::new(single_package, Some(release_commit)),
                    );
                }
            }
        }

        worktrees.iter().for_each(|x| info!("{:?}", x.path()));
        Ok(PackagesCollection::default().with_packages(res))
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

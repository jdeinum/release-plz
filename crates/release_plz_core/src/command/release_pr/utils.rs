use crate::command::release_pr::git::{CustomRepo, CustomWorkTree};
use anyhow::{Context, Result, bail};
use bytes::{BufMut, BytesMut};
use cargo::{ops::info, util::hex};
use cargo_metadata::{Metadata, MetadataCommand, Package, PackageName, camino::Utf8Path};
use ignore::{WalkBuilder, types::Types};
use regex::Regex;
use std::{
    io::{Read, Stdin, Stdout},
    path::Path,
    process::Stdio,
};
use tracing::info;

/// To determine when packages have changed, we'll use a simple MD5 hash
/// It's fast, and doesn't need to be cryptocraphically secure
#[derive(Debug, Default, Clone)]
pub struct MD5Hash([u8; 16]);

/// Get the MD5 hash for a directory
pub fn hash_dir_md5(path: impl AsRef<Utf8Path>) -> Result<MD5Hash> {
    let cleaned_path = Path::new(path.as_ref().as_str());

    info!("getting md5 of {cleaned_path:?}");

    // ensure the directory exists
    if !cleaned_path.exists() {
        bail!("{cleaned_path:?} does not exist");
    }

    // make sure its a directory
    if !cleaned_path.is_dir() {
        bail!("{cleaned_path:?} is not a directory");
    }

    // hash it all!
    let mut all_bytes: BytesMut = BytesMut::new();
    let mut buffer: Vec<u8> = Vec::new();
    for path in WalkBuilder::new(cleaned_path)
        .hidden(false)
        .git_global(false)
        .git_exclude(false)
        .git_ignore(false)
        .build()
    {
        let path = path.context("get path")?;
        info!("hashing path: {path:?}");
        let mut f = std::fs::File::open(path.path()).context("open file to read bytes")?;
        if f.metadata().unwrap().is_file() {
            buffer.drain(..);
            f.read_to_end(&mut buffer)
                .context("read all bytes in file")?;
            all_bytes.put_slice(&buffer);
        }
    }
    let hash = md5::compute(&all_bytes.to_vec());
    Ok(MD5Hash(hash.0))
}

/// Represents a package as far as its on disk representation is concerned.
/// The metadata is stored within the package, while the MD5 hash lets us know when contents change
/// This is the only mechanism we use to compare two packages
#[derive(Debug)]
pub(crate) struct PackageState {
    pub metadata: Metadata,
    pub md5_hash: MD5Hash,
}

/// Regardless of where our package originates from, we have to be able to transform it into a
/// package state so they are comparible
/// By default they'll be unimplemented so that if you try to convert a non git package into a
/// PackageState with the visit_git_package, you'll error out.
pub(crate) trait ToPackage {
    fn try_visit_git_package(self, _worktree: CustomWorkTree) -> Result<PackageState>
    where
        Self: Sized,
    {
        bail!("unimplemented");
    }
    fn try_visit_registry_package(self) -> Result<PackageState>
    where
        Self: Sized,
    {
        bail!("unimplemented");
    }
}

pub struct GitPackage {
    pub package_name: PackageName,
    pub git_tag_release_regex: Regex,
}

impl ToPackage for GitPackage {
    fn try_visit_git_package(self, worktree: CustomWorkTree) -> Result<PackageState> {
        // since we already have the repository, we need to first find the commit for our release
        let mut repo = CustomRepo::open(worktree.path()).context("open worktree")?;

        // get all of the tags for this repository
        let tags = repo.get_tags().context("get tags")?;
        info!("Found tags: {tags:?}");

        // now, we iterate through the tags to see if we have a match
        let release: Option<String> = tags
            .into_iter()
            .find(|tag| self.git_tag_release_regex.is_match(&tag));

        match release {
            Some(release_tag) => {
                info!(
                    "Tag {release_tag} matched release regex {} for package {}",
                    self.git_tag_release_regex, self.package_name
                );

                // find the commit associated with the tag
                let commit = repo
                    .get_tag_commit(&release_tag)
                    .context("get commit for release tag")?;
                info!("Found commit {commit} for tag {release_tag}");

                // checkout to that commit
                repo.checkout_commit(&commit)
                    .context("checkout release commit")?;

                // run cargo publish
                let c = std::process::Command::new("cargo")
                    .args(["package"])
                    .current_dir(worktree.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .context("run cargo package in worktree")?;

                let _ = c.wait_with_output().context("wait for cargo package")?;

                // now, we have to get the package details to get the final path, to do this, we
                // create a rust package
                let rust_package = MetadataCommand::new()
                    .manifest_path(format!(
                        "{}/Cargo.toml",
                        worktree.path().to_str().unwrap_or_default()
                    ))
                    .exec()
                    .context("get cargo metadata")?;

                // get the details about the package version
                let package_details = rust_package
                    .packages
                    .iter()
                    .filter(|x| x.name == self.package_name)
                    .next()
                    .unwrap();

                let new_path = format!(
                    "{}/target/package/{}-{}",
                    worktree.path().to_str().unwrap_or_default(),
                    package_details.name,
                    package_details.version
                );
                info!("package for {} is at {}", self.package_name, new_path);

                // get the md5 for this package
                let md5_hash = hash_dir_md5(&new_path).context("get md5 hash for package")?;

                // single package metadata
                let single_package_metadata = MetadataCommand::new()
                    .manifest_path(format!("{}/Cargo.toml", new_path))
                    .exec()
                    .context("get cargo metadata")?;

                info!("md5 hash for {} is {md5_hash:?}", self.package_name,);

                return Ok(PackageState {
                    metadata: single_package_metadata,
                    md5_hash,
                });
            }

            None => {
                unimplemented!(
                    "If we don't find a release commit, we need to find the inception commit instead!"
                );
            }
        }
    }
}

pub struct RegistryPackage {
    pub package_name: PackageName,
}

impl ToPackage for RegistryPackage {
    fn try_visit_registry_package(self) -> Result<PackageState> {
        todo!()
    }
}

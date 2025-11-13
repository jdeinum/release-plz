use anyhow::{Context, Result, bail};
use git2::{Oid, Repository, Worktree, WorktreePruneOptions};
use std::path::Path;
use tracing::{error, info, warn};

pub struct CustomRepo {
    repo: Repository,
}

impl CustomRepo {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        info!("Opening git repo at {:?}", path.as_ref());
        let repo = Repository::open(path).context("open repository")?;
        Ok(Self { repo })
    }

    pub fn temp_worktree(
        &self,
        path: Option<impl AsRef<Path>>,
        name: &str,
    ) -> Result<CustomWorkTree> {
        let path_name_suffix: String = if let Some(p) = path {
            p.as_ref().to_str().unwrap().to_string()
        } else {
            chrono::Utc::now().format("%d_%m_%Y_%H_%M").to_string()
        };
        let path_name = format!("/tmp/{path_name_suffix}");
        let path = Path::new(&path_name);

        let trees: Vec<String> = self
            .repo
            .worktrees()
            .context("get worktrees for repo")?
            .iter()
            .filter(|x| x.is_some())
            .map(|y| y.unwrap().to_string())
            .collect();

        if trees.contains(&name.to_string()) {
            info!("worktree {name} already present, deleting it first");
            let wt = self
                .repo
                .find_worktree(name)
                .context("find worktree to delete it ")?;
            drop(wt);
        }

        info!("Creating worktree called {name} at {path_name}");
        let wt = self
            .repo
            .worktree(name, path, None)
            .context("create worktree")?;
        Ok(CustomWorkTree { worktree: wt })
    }

    pub fn get_tags(&self) -> Result<Vec<String>> {
        let tags: Vec<String> = self
            .repo
            .tag_names(None)
            .context("get tags for repo")?
            .iter()
            .filter(|x| x.is_some())
            .map(|y| y.unwrap().to_string())
            .collect();
        Ok(tags)
    }

    pub fn get_tag_commit(&self, tag_name: &str) -> Result<String> {
        let mut commit: Option<Oid> = None;
        self.repo
            .tag_foreach(|a, _| {
                let t = match self.repo.find_tag(a) {
                    Ok(t) => t,
                    Err(e) => {
                        error!("Error finding tag: {e:?}");
                        return true;
                    }
                };

                if t.name().unwrap_or_default() == tag_name {
                    commit = Some(t.target().unwrap().into_commit().unwrap().id());
                }

                true
            })
            .context("find commit for tag")?;

        if let Some(id) = commit {
            Ok(id.to_string())
        } else {
            bail!("No commit associated with tag")
        }
    }

    pub fn checkout_commit(&mut self, commit_sha: &str) -> Result<()> {
        // first we convert the string to an Oid
        let id = Oid::from_str(commit_sha).context("convert commit sha to oid")?;
        self.repo
            .set_head_detached(id)
            .context("set head to detached commit")?;
        info!("Checked out commit {commit_sha}");
        Ok(())
    }

    pub fn path(&self) -> &Path {
        self.repo.path()
    }

    pub fn delete_branch(&mut self, branch_name: &str) -> Result<()> {
        let mut branch = self
            .repo
            .find_branch(branch_name, git2::BranchType::Local)
            .context("find local branch")?;
        branch.delete().context("delete branch")?;
        Ok(())
    }
}

pub struct CustomWorkTree {
    worktree: Worktree,
}

impl Drop for CustomWorkTree {
    fn drop(&mut self) {
        // open a repo so we can delete the branch, would be better to hold a reference back to the
        // original repo to do it for us (similar to RefCell), but this will suffice for now
        let mut repo = match CustomRepo::open(self.path()) {
            Ok(r) => r,
            Err(e) => {
                error!("Error creating repo to drop branch: {e:?}");
                return;
            }
        };

        // change to head so we can delete this branch
        // TODO: Find cleaner way to do this
        repo.repo
            .set_head_detached(repo.repo.head().unwrap().target().unwrap());

        match repo.delete_branch(self.worktree.name().unwrap_or_default()) {
            Ok(_) => {}
            Err(e) => error!("Error deleting branch: {e:?}"),
        };

        // death to the trees!
        if let Err(e) = self.worktree.prune(Some(
            WorktreePruneOptions::new().working_tree(true).valid(true),
        )) {
            warn!("Couldn't prune worktree: {e:?}");
        }
    }
}

impl CustomWorkTree {
    pub fn path(&self) -> &Path {
        self.worktree.path()
    }
}

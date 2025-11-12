use cargo_metadata::camino::Utf8PathBuf;
use next_version::VersionUpdater;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateConfig {
    /// This path needs to be a relative path to the Cargo.toml of the project.
    /// I.e. if you have a workspace, it needs to be relative to the workspace root.
    pub changelog_path: Option<Utf8PathBuf>,
    /// Controls when to run cargo-semver-checks.
    /// Note: You can only run cargo-semver-checks if the package contains a library.
    ///       For example, if it has a `lib.rs` file.
    pub semver_check: bool,
    /// Whether to create/update changelog or not.
    /// Default: `true`.
    pub changelog_update: bool,
    /// High-level toggle to process this package or ignore it.
    pub release: bool,
    /// Whether to publish this package to a registry.
    /// Default: `true`.
    pub publish: bool,
    /// - If `true`, feature commits will always bump the minor version, even in 0.x releases.
    /// - If `false` (default), feature commits will only bump the minor version starting with 1.x releases.
    pub features_always_increment_minor: bool,

    /// Template for the git tag created by release-plz.
    pub tag_name_template: Option<String>,

    /// Use git tags for release information
    /// Default: None
    ///
    /// If Some(x), release-plz will use git tags to determine what the latest version of the package
    /// is (i.e newest version is v.0.1.3 and is associated with commit ac83762)
    /// If None, release-plz will use crates.io release information to get the latest version
    ///
    /// x must be a valid regular expression that identifies releases for packages
    /// Because git tags can have arbitrary messages attached to them, release-plz needs some way
    /// of knowing the difference between a release and non release.
    ///
    /// NOTE: The tag needs to contain a semantic version somewhere in the tag. If it doesn't, we
    /// should error out early. A better option may be that we can specify a prefix and postfix as
    /// separate fields and expect a full a.b.c in the middle, that way users can never forget it
    pub git_only_release_regex: Option<String>,
}

/// Package-specific config
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageUpdateConfig {
    /// config that can be applied by default to all packages.
    pub generic: UpdateConfig,
    /// List of package names.
    /// Include the changelogs of these packages in the changelog of the current package.
    pub changelog_include: Vec<String>,
    pub version_group: Option<String>,
}

impl From<UpdateConfig> for PackageUpdateConfig {
    fn from(config: UpdateConfig) -> Self {
        Self {
            generic: config,
            changelog_include: vec![],
            version_group: None,
        }
    }
}

impl PackageUpdateConfig {
    pub fn semver_check(&self) -> bool {
        self.generic.semver_check
    }

    pub fn should_update_changelog(&self) -> bool {
        self.generic.changelog_update
    }

    pub fn should_publish(&self) -> bool {
        self.generic.publish
    }
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            semver_check: true,
            changelog_update: true,
            release: true,
            publish: true,
            features_always_increment_minor: false,
            tag_name_template: None,
            changelog_path: None,
            git_only_release_regex: None,
        }
    }
}

impl UpdateConfig {
    pub fn with_semver_check(self, semver_check: bool) -> Self {
        Self {
            semver_check,
            ..self
        }
    }

    pub fn with_features_always_increment_minor(
        self,
        features_always_increment_minor: bool,
    ) -> Self {
        Self {
            features_always_increment_minor,
            ..self
        }
    }

    pub fn with_changelog_update(self, changelog_update: bool) -> Self {
        Self {
            changelog_update,
            ..self
        }
    }

    pub fn with_publish(self, publish: bool) -> Self {
        Self { publish, ..self }
    }

    pub fn version_updater(&self) -> VersionUpdater {
        VersionUpdater::default()
            .with_features_always_increment_minor(self.features_always_increment_minor)
    }
}

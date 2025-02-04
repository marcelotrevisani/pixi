pub mod manifest;
pub mod metadata;

use indexmap::IndexMap;
use itertools::Itertools;
use miette::{IntoDiagnostic, NamedSource, WrapErr};
use once_cell::sync::OnceCell;
use rattler_conda_types::{Channel, MatchSpec, NamelessMatchSpec, PackageName, Platform, Version};
use rattler_virtual_packages::VirtualPackage;
use rip::{index::PackageDb, normalize_index_url};
use std::collections::HashMap;
use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    consts::{self, PROJECT_MANIFEST},
    default_client,
    task::Task,
    virtual_packages::non_relevant_virtual_packages_for_platform,
};
use manifest::{Manifest, PyPiRequirement, SystemRequirements};
use rip::types::NormalizedPackageName;
use std::fmt::{Display, Formatter};
use url::Url;

/// The dependency types we support
#[derive(Debug, Copy, Clone)]
pub enum DependencyType {
    CondaDependency(SpecType),
    PypiDependency,
}

impl DependencyType {
    /// Convert to a name used in the manifest
    pub fn name(&self) -> &'static str {
        match self {
            DependencyType::CondaDependency(dep) => dep.name(),
            DependencyType::PypiDependency => consts::PYPI_DEPENDENCIES,
        }
    }
}
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
/// What kind of dependency spec do we have
pub enum SpecType {
    /// Host dependencies are used that are needed by the host environment when running the project
    Host,
    /// Build dependencies are used when we need to build the project, may not be required at runtime
    Build,
    /// Regular dependencies that are used when we need to run the project
    Run,
}

impl SpecType {
    /// Convert to a name used in the manifest
    pub fn name(&self) -> &'static str {
        match self {
            SpecType::Host => "host-dependencies",
            SpecType::Build => "build-dependencies",
            SpecType::Run => "dependencies",
        }
    }
}

/// The pixi project, this main struct to interact with the project. This struct holds the [`Manifest`] and has functions to modify or request information from it.
/// This allows in the future to have multiple environments or manifests linked to a project.
#[derive(Clone)]
pub struct Project {
    /// Root folder of the project
    root: PathBuf,
    /// The PyPI package db for this project
    package_db: OnceCell<Arc<PackageDb>>,
    /// The manifest for the project
    pub(crate) manifest: Manifest,
}

impl Project {
    /// Constructs a new instance from an internal manifest representation
    pub fn from_manifest(manifest: Manifest) -> Self {
        Self {
            root: Default::default(),
            package_db: Default::default(),
            manifest,
        }
    }

    /// Discovers the project manifest file in the current directory or any of the parent
    /// directories.
    /// This will also set the current working directory to the project root.
    pub fn discover() -> miette::Result<Self> {
        let project_toml = match find_project_root() {
            Some(root) => root.join(PROJECT_MANIFEST),
            None => miette::bail!("could not find {}", PROJECT_MANIFEST),
        };
        Self::load(&project_toml)
    }

    /// Returns the source code of the project as [`NamedSource`].
    /// Used in error reporting.
    pub fn manifest_named_source(&self) -> NamedSource {
        NamedSource::new(PROJECT_MANIFEST, self.manifest.contents.clone())
    }

    /// Loads a project from manifest file.
    fn load(manifest_path: &Path) -> miette::Result<Self> {
        // Determine the parent directory of the manifest file
        let full_path = dunce::canonicalize(manifest_path).into_diagnostic()?;
        if full_path.file_name().and_then(OsStr::to_str) != Some(PROJECT_MANIFEST) {
            miette::bail!("the manifest-path must point to a {PROJECT_MANIFEST} file");
        }

        let root = full_path
            .parent()
            .ok_or_else(|| miette::miette!("can not find parent of {}", manifest_path.display()))?;

        // Load the TOML document
        let manifest = fs::read_to_string(manifest_path)
            .into_diagnostic()
            .and_then(|content| Manifest::from_str(root, content))
            .wrap_err_with(|| {
                format!(
                    "failed to parse {} from {}",
                    consts::PROJECT_MANIFEST,
                    root.display()
                )
            });

        Ok(Self {
            root: root.to_owned(),
            package_db: Default::default(),
            manifest: manifest?,
        })
    }

    /// Loads a project manifest file or discovers it in the current directory or any of the parent
    pub fn load_or_else_discover(manifest_path: Option<&Path>) -> miette::Result<Self> {
        let project = match manifest_path {
            Some(path) => Project::load(path)?,
            None => Project::discover()?,
        };
        Ok(project)
    }

    /// Returns the name of the project
    pub fn name(&self) -> &str {
        &self.manifest.parsed.project.name
    }

    /// Returns the version of the project
    pub fn version(&self) -> &Option<Version> {
        &self.manifest.parsed.project.version
    }

    /// Returns the description of the project
    pub fn description(&self) -> &Option<String> {
        &self.manifest.parsed.project.description
    }

    /// Returns the root directory of the project
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the pixi directory
    pub fn pixi_dir(&self) -> PathBuf {
        self.root.join(consts::PIXI_DIR)
    }

    /// Returns the environment directory
    pub fn environment_dir(&self) -> PathBuf {
        self.pixi_dir().join(consts::ENVIRONMENT_DIR)
    }

    /// Returns the path to the manifest file.
    pub fn manifest_path(&self) -> PathBuf {
        self.manifest.path.clone()
    }

    /// Returns the path to the lock file of the project
    pub fn lock_file_path(&self) -> PathBuf {
        self.root.join(consts::PROJECT_LOCK_FILE)
    }

    /// Save back changes
    pub fn save(&mut self) -> miette::Result<()> {
        self.manifest.save()
    }

    /// Returns the channels used by this project
    pub fn channels(&self) -> &[Channel] {
        &self.manifest.parsed.project.channels
    }

    /// Returns the platforms this project targets
    pub fn platforms(&self) -> &[Platform] {
        self.manifest.parsed.project.platforms.as_ref().as_slice()
    }

    /// Get the tasks of this project
    pub fn tasks(&self, platform: Option<Platform>) -> HashMap<&str, &Task> {
        self.manifest.tasks(platform)
    }

    /// Get the task with the specified `name` or `None` if no such task exists. If `platform` is
    /// specified then the task will first be looked up in the target specific tasks for the given
    /// platform.
    pub fn task_opt(&self, name: &str, platform: Option<Platform>) -> Option<&Task> {
        self.manifest.tasks(platform).get(name).copied()
    }

    /// Returns all tasks defined in the project for the given platform
    pub fn task_names(&self, platform: Option<Platform>) -> Vec<&str> {
        self.manifest.tasks(platform).keys().copied().collect_vec()
    }

    /// Returns names of the tasks that depend on the given task.
    pub fn task_names_depending_on(&self, name: impl AsRef<str>) -> Vec<&str> {
        let mut tasks = self.manifest.tasks(Some(Platform::current()));
        let task = tasks.remove(name.as_ref());
        if task.is_some() {
            tasks
                .into_iter()
                .filter(|(_, c)| c.depends_on().contains(&name.as_ref().to_string()))
                .map(|(name, _)| name)
                .collect()
        } else {
            vec![]
        }
    }

    /// Returns the dependencies of the project.
    pub fn dependencies(
        &self,
        platform: Platform,
        kind: SpecType,
    ) -> IndexMap<PackageName, NamelessMatchSpec> {
        self.manifest
            .default_feature()
            .targets
            .resolve(Some(platform))
            .collect_vec()
            .into_iter()
            .rev() // We rev this so that the most specific target is last.
            .flat_map(|t| t.dependencies.get(&kind).into_iter().flatten())
            .map(|(name, spec)| (name.clone(), spec.clone()))
            .collect()
    }

    /// Returns all dependencies of the project. These are the run, host, build dependency sets combined.
    pub fn all_dependencies(&self, platform: Platform) -> IndexMap<PackageName, NamelessMatchSpec> {
        let mut dependencies = self.dependencies(platform, SpecType::Run);
        dependencies.extend(self.dependencies(platform, SpecType::Host));
        dependencies.extend(self.dependencies(platform, SpecType::Build));
        dependencies
    }

    pub fn pypi_dependencies(
        &self,
        platform: Platform,
    ) -> IndexMap<rip::types::PackageName, PyPiRequirement> {
        self.manifest
            .default_feature()
            .targets
            .resolve(Some(platform))
            .collect_vec()
            .into_iter()
            .rev() // We rev this so that the most specific target is last.
            .flat_map(|t| t.pypi_dependencies.iter().flatten())
            .map(|(name, spec)| (name.clone(), spec.clone()))
            .collect()
    }

    /// Returns true if the project contains any pypi dependencies
    pub fn has_pypi_dependencies(&self) -> bool {
        self.manifest.has_pypi_dependencies()
    }

    /// Returns the Python index URLs to use for this project.
    pub fn pypi_index_urls(&self) -> Vec<Url> {
        let index_url = normalize_index_url(Url::parse("https://pypi.org/simple/").unwrap());
        vec![index_url]
    }

    /// Returns the package database used for caching python metadata, wheels and more. See the
    /// documentation of [`rip::index::PackageDb`] for more information.
    pub fn pypi_package_db(&self) -> miette::Result<&PackageDb> {
        Ok(self
            .package_db
            .get_or_try_init(|| {
                PackageDb::new(
                    default_client(),
                    &self.pypi_index_urls(),
                    &rattler::default_cache_dir()
                        .map_err(|_| {
                            miette::miette!("could not determine default cache directory")
                        })?
                        .join("pypi/"),
                )
                .into_diagnostic()
                .map(Arc::new)
            })?
            .as_ref())
    }

    /// Returns the all specified activation scripts that are used in the current platform.
    pub fn activation_scripts(&self, platform: Platform) -> miette::Result<Vec<PathBuf>> {
        let feature = self.manifest.default_feature();

        // Select the platform-specific activation scripts that is most specific
        let activation = feature
            .targets
            .resolve(Some(platform))
            .filter_map(|target| target.activation.as_ref())
            .next();

        // Get the activation scripts
        let all_scripts = activation
            .into_iter()
            .flat_map(|activation| activation.scripts.iter().flatten())
            .collect_vec();

        // Check if scripts exist
        let mut full_paths = Vec::new();
        let mut missing_scripts = Vec::new();
        for script_name in &all_scripts {
            let script_path = self.root().join(script_name);
            if script_path.exists() {
                full_paths.push(script_path);
                tracing::debug!("Found activation script: {:?}", script_name);
            } else {
                missing_scripts.push(script_name);
            }
        }

        if !missing_scripts.is_empty() {
            tracing::warn!("can't find activation scripts: {:?}", missing_scripts);
        }

        Ok(full_paths)
    }

    /// Get the system requirements defined under the `system-requirements` section of the project manifest.
    /// They will act as the description of a reference machine which is minimally needed for this package to be run.
    pub fn system_requirements(&self) -> &SystemRequirements {
        &self.manifest.default_feature().system_requirements
    }

    /// Get the system requirements defined under the `system-requirements` section of the project manifest.
    /// Excluding packages that are not relevant for the specified platform.
    pub fn virtual_packages_for_platform(&self, platform: Platform) -> Vec<VirtualPackage> {
        // Filter system requirements based on the relevant packages for the current OS.
        self.system_requirements()
            .virtual_packages()
            .iter()
            .filter(|requirement| {
                !non_relevant_virtual_packages_for_platform(requirement, platform)
            })
            .cloned()
            .collect()
    }
}

/// Iterates over the current directory and all its parent directories and returns the first
/// directory path that contains the [`consts::PROJECT_MANIFEST`].
pub fn find_project_root() -> Option<PathBuf> {
    let current_dir = env::current_dir().ok()?;
    std::iter::successors(Some(current_dir.as_path()), |prev| prev.parent())
        .find(|dir| dir.join(consts::PROJECT_MANIFEST).is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::{assert_debug_snapshot, assert_display_snapshot};
    use rattler_virtual_packages::{LibC, VirtualPackage};
    use std::str::FromStr;

    const PROJECT_BOILERPLATE: &str = r#"
        [project]
        name = "foo"
        version = "0.1.0"
        channels = []
        platforms = ["linux-64", "win-64"]
        "#;

    #[test]
    fn test_system_requirements_edge_cases() {
        let file_contents = [
            r#"
        [system-requirements]
        libc = { version = "2.12" }
        "#,
            r#"
        [system-requirements]
        libc = "2.12"
        "#,
            r#"
        [system-requirements.libc]
        version = "2.12"
        "#,
            r#"
        [system-requirements.libc]
        version = "2.12"
        family = "glibc"
        "#,
        ];

        for file_content in file_contents {
            let file_content = format!("{PROJECT_BOILERPLATE}\n{file_content}");

            let manifest = Manifest::from_str(Path::new(""), &file_content).unwrap();
            let project = Project::from_manifest(manifest);
            let expected_result = vec![VirtualPackage::LibC(LibC {
                family: "glibc".to_string(),
                version: Version::from_str("2.12").unwrap(),
            })];

            let virtual_packages = project.system_requirements().virtual_packages();

            assert_eq!(virtual_packages, expected_result);
        }
    }

    fn format_dependencies(deps: IndexMap<PackageName, NamelessMatchSpec>) -> String {
        deps.iter()
            .map(|(name, spec)| format!("{} = \"{}\"", name.as_source(), spec))
            .join("\n")
    }

    #[test]
    fn test_dependency_sets() {
        let file_contents = r#"
        [dependencies]
        foo = "1.0"

        [host-dependencies]
        libc = "2.12"

        [build-dependencies]
        bar = "1.0"
        "#;

        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();
        let project = Project::from_manifest(manifest);

        assert_display_snapshot!(format_dependencies(
            project.all_dependencies(Platform::Linux64)
        ));
    }

    #[test]
    fn test_dependency_target_sets() {
        let file_contents = r#"
        [dependencies]
        foo = "1.0"

        [host-dependencies]
        libc = "2.12"

        [build-dependencies]
        bar = "1.0"

        [target.linux-64.build-dependencies]
        baz = "1.0"

        [target.linux-64.host-dependencies]
        banksy = "1.0"

        [target.linux-64.dependencies]
        wolflib = "1.0"
        "#;
        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();
        let project = Project::from_manifest(manifest);

        assert_display_snapshot!(format_dependencies(
            project.all_dependencies(Platform::Linux64)
        ));
    }

    #[test]
    fn test_activation_scripts() {
        fn fmt_activation_scripts(scripts: Vec<PathBuf>) -> String {
            scripts
                .iter()
                .format_with("\n", |p, f| f(&format_args!("{}", p.display())))
                .to_string()
        }

        // Using known files in the project so the test succeed including the file check.
        let file_contents = r#"
            [target.linux-64.activation]
            scripts = ["Cargo.toml"]

            [target.win-64.activation]
            scripts = ["Cargo.lock"]

            [activation]
            scripts = ["pixi.toml", "pixi.lock"]
            "#;
        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();
        let project = Project::from_manifest(manifest);

        assert_display_snapshot!(format!(
            "= Linux64\n{}\n\n= Win64\n{}\n\n= OsxArm64\n{}",
            fmt_activation_scripts(project.activation_scripts(Platform::Linux64).unwrap()),
            fmt_activation_scripts(project.activation_scripts(Platform::Win64).unwrap()),
            fmt_activation_scripts(project.activation_scripts(Platform::OsxArm64).unwrap())
        ));
    }

    #[test]
    fn test_target_specific_tasks() {
        // Using known files in the project so the test succeed including the file check.
        let file_contents = r#"
            [tasks]
            test = "test multi"

            [target.win-64.tasks]
            test = "test win"

            [target.linux-64.tasks]
            test = "test linux"
            "#;
        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();

        let project = Project::from_manifest(manifest);

        assert_debug_snapshot!(project.manifest.tasks(Some(Platform::Osx64)));
        assert_debug_snapshot!(project.manifest.tasks(Some(Platform::Win64)));
        assert_debug_snapshot!(project.manifest.tasks(Some(Platform::Linux64)));
    }
}

#[derive(Eq, PartialEq, Hash)]
pub enum DependencyName {
    Conda(PackageName),
    PyPi(NormalizedPackageName),
}

#[derive(Clone)]
pub enum DependencyKind {
    Conda(MatchSpec),
    PyPi(pep508_rs::Requirement),
}

impl Display for DependencyKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DependencyKind::Conda(spec) => write!(f, "{}", spec),
            DependencyKind::PyPi(req) => write!(f, "{}", req),
        }
    }
}

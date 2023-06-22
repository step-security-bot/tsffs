//! Confuse Simics Project
//!
//! This crate provides tools for managing simics projects, including linking to simics, loading
//! modules, and creating and destroying temporary project directories, and actually running
//! the SIMICS process after configuration

use crate::{
    module::Module,
    package::{Package, PackageBuilder, PublicPackageNumber},
    simics::home::simics_home,
    traits::Setup,
    util::copy_dir_contents,
};
use anyhow::{anyhow, bail, ensure, Error, Result};
use derive_builder::Builder;
use simics_api::sys::SIMICS_VERSION;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    fs::{copy, create_dir_all, read_to_string, remove_dir_all, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};
use strum::{AsRefStr, Display};
use tempdir::TempDir;
use tracing::{debug, error, info};
use version_tools::VersionConstraint;

#[derive(Debug, Clone)]
pub struct SimicsPath {
    from: Option<SimicsPathMarker>,
    to: PathBuf,
}

impl SimicsPath {
    fn new<P: AsRef<Path>>(p: P, from: Option<SimicsPathMarker>) -> Self {
        if from.is_some() {
            let to = p.as_ref().to_path_buf().components().skip(1).collect();
            Self { from, to }
        } else {
            Self {
                from: None,
                to: p.as_ref().to_path_buf(),
            }
        }
    }

    pub fn simics<P: AsRef<Path>>(p: P) -> Self {
        Self::new(p, Some(SimicsPathMarker::Simics))
    }

    pub fn script<P: AsRef<Path>>(p: P) -> Self {
        Self::new(p, Some(SimicsPathMarker::Script))
    }

    pub fn path<P: AsRef<Path>>(p: P) -> Self {
        Self::new(p, None)
    }

    pub fn canonicalize<P: AsRef<Path>>(&self, base: P) -> Result<PathBuf> {
        debug!(
            "Canonicalizing {:?} on base {}",
            self,
            base.as_ref().display()
        );
        let canonicalized = match self.from {
            Some(SimicsPathMarker::Script) => bail!("Script relative paths are not supported"),
            Some(SimicsPathMarker::Simics) => base
                .as_ref()
                .to_path_buf()
                .canonicalize()
                .map_err(|e| {
                    anyhow!(
                        "Failed to canonicalize base path {}: {}",
                        base.as_ref().to_path_buf().display(),
                        e
                    )
                })?
                .join(&self.to),
            None => self.to.clone(),
        };
        debug!(
            "Canonicalized simics path {:?} to {}",
            self,
            canonicalized.display()
        );
        Ok(canonicalized)
    }
}

impl From<PathBuf> for SimicsPath {
    fn from(value: PathBuf) -> Self {
        Self::path(value)
    }
}

impl FromStr for SimicsPath {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let p = PathBuf::from(s);
        Ok(match p.components().next() {
            Some(c) if c.as_os_str() == SimicsPathMarker::Script.as_ref() => Self::script(s),
            Some(c) if c.as_os_str() == SimicsPathMarker::Simics.as_ref() => Self::simics(s),
            _ => Self::path(
                PathBuf::from(s)
                    .canonicalize()
                    .map_err(|e| anyhow!("Failed to canonicalize simics path {}: {}", s, e))?,
            ),
        })
    }
}

impl TryFrom<&str> for SimicsPath {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        value.parse()
    }
}

#[derive(Debug, Clone, AsRefStr, Display)]
enum SimicsPathMarker {
    /// `%simics%`
    #[strum(serialize = "%simics%")]
    Simics,
    /// `%script%`
    #[strum(serialize = "%script%")]
    Script,
}

#[derive(Builder, Debug, Clone)]
#[builder(build_fn(error = "Error"))]
pub struct ProjectPath {
    #[builder(setter(into), default = "self.default_path()?")]
    pub path: PathBuf,
    #[builder(default = "true")]
    temporary: bool,
}

impl ProjectPathBuilder {
    fn default_path(&self) -> Result<PathBuf> {
        let path = TempDir::new(ProjectPath::PREFIX)?.into_path();
        Ok(path)
    }
}

impl ProjectPath {
    const PREFIX: &str = "project";

    fn default_path() -> Result<PathBuf> {
        Ok(TempDir::new(Self::PREFIX)?.into_path())
    }

    fn default() -> Result<Self> {
        Ok(Self {
            path: Self::default_path()?,
            temporary: true,
        })
    }
}

impl<P> From<P> for ProjectPath
where
    P: AsRef<Path>,
{
    fn from(value: P) -> Self {
        Self {
            path: value.as_ref().to_path_buf(),
            temporary: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct PropertiesMd5Entry {
    path: String,
    // Always 'MD5'
    hash_type: String,
    hash: String,
}

impl PropertiesMd5Entry {
    pub const SEPARATOR: &str = "MD5";
}

impl FromStr for PropertiesMd5Entry {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let cols = s
            .split(Self::SEPARATOR)
            .map(|c| c.trim())
            .collect::<Vec<_>>();
        Ok(Self {
            path: cols
                .first()
                .ok_or_else(|| anyhow!("No path column in {}", s))?
                .to_string(),
            hash_type: Self::SEPARATOR.to_string(),
            hash: cols
                .get(1)
                .ok_or_else(|| anyhow!("No hash column in {}", s))?
                .to_string(),
        })
    }
}

pub struct PropertiesMd5 {
    _md5: HashSet<PropertiesMd5Entry>,
}

impl FromStr for PropertiesMd5 {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(Self {
            _md5: s
                .lines()
                .filter_map(|l| {
                    l.parse()
                        .map_err(|e| {
                            error!("Error parsing line {} into md5 entry", e);
                            e
                        })
                        .ok()
                })
                .collect(),
        })
    }
}

pub struct PropertiesPaths {
    _project: String,
    simics_root: String,
    _simics_model_builder: String,
    _mingw: String,
}

impl FromStr for PropertiesPaths {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let paths = s
            .lines()
            .map(|l| l.split(':').map(|l| l.trim()).collect::<Vec<_>>())
            .map(|l| {
                (
                    l.first().map(|k| k.to_string()).unwrap_or("".to_owned()),
                    l.get(1).map(|v| v.to_string()).unwrap_or("".to_owned()),
                )
            })
            .collect::<HashMap<_, _>>();
        Ok(Self {
            _project: paths
                .get("project")
                .cloned()
                .ok_or_else(|| anyhow!("No field project in {}", s))?,
            simics_root: paths
                .get("simics-root")
                .cloned()
                .ok_or_else(|| anyhow!("No field simics-root in {}", s))?,
            _simics_model_builder: paths
                .get("simics-model-builder")
                .cloned()
                .ok_or_else(|| anyhow!("No field simics-model-builder in {}", s))?,
            _mingw: paths
                .get("mingw")
                .cloned()
                .ok_or_else(|| anyhow!("No field mingw in {}", s))?,
        })
    }
}

pub struct Properties {
    _md5: PropertiesMd5,
    paths: PropertiesPaths,
}

impl TryFrom<PathBuf> for Properties {
    type Error = Error;
    fn try_from(value: PathBuf) -> Result<Self> {
        Self::try_from(&value)
    }
}
impl TryFrom<&PathBuf> for Properties {
    type Error = Error;
    fn try_from(value: &PathBuf) -> Result<Self> {
        let properties_dir = value.join(".project-properties");
        let md5_path = properties_dir.join("project-md5");
        let paths_path = properties_dir.join("project-paths");
        Ok(Self {
            _md5: read_to_string(md5_path)?.parse()?,
            paths: read_to_string(paths_path)?.parse()?,
        })
    }
}

#[derive(Builder, Clone)]
#[builder(build_fn(error = "Error"))]
pub struct Project {
    #[builder(setter(into), default = "self.default_path()?")]
    /// The path to the project base directory.
    pub path: ProjectPath,
    #[builder(setter(into), default = "self.default_base()?")]
    /// The base version constraint to use when building the project. You should never
    /// have to specify this.
    base: Package,
    #[builder(setter(into), default = "self.default_home()?")]
    /// The SIMICS Home directory. You should never need to manually specify this.
    home: PathBuf,
    #[builder(setter(each(name = "package", into), into), default)]
    packages: HashSet<Package>,
    #[builder(setter(each(name = "module", into), into), default)]
    modules: HashSet<Module>,
    #[builder(setter(each(name = "directory", into), into), default)]
    directories: HashMap<PathBuf, SimicsPath>,
    #[builder(setter(each(name = "file", into), into), default)]
    files: HashMap<PathBuf, SimicsPath>,
    #[builder(setter(each(name = "file_content", into), into), default)]
    file_contents: HashMap<Vec<u8>, SimicsPath>,
    #[builder(setter(each(name = "path_symlink", into), into), default)]
    path_symlinks: HashMap<PathBuf, SimicsPath>,
}

impl TryFrom<PathBuf> for Project {
    type Error = Error;

    /// Initialize a project from an existing project on disk
    fn try_from(value: PathBuf) -> Result<Self> {
        let properties = Properties::try_from(&value)?;
        let simics_root = PathBuf::from(&properties.paths.simics_root);
        let home = simics_root
            .parent()
            .ok_or_else(|| anyhow!("No parent found for {}", properties.paths.simics_root))?;
        let base = Package::try_from(PathBuf::from(properties.paths.simics_root))?;
        let packages = read_to_string(value.join(".package-list"))?
            .lines()
            .filter_map(|l| {
                PathBuf::from(l)
                    .canonicalize()
                    .map_err(|e| {
                        anyhow!("Error canonicalizing package list entry path {}: {}", l, e)
                    })
                    .ok()
            })
            .map(Package::try_from)
            .filter_map(|p| {
                p.map_err(|e| {
                    error!("Error parsing package: {}", e);
                    e
                })
                .ok()
            })
            .collect::<HashSet<_>>();
        Ok(Self {
            path: value.into(),
            base,
            home: home.to_path_buf(),
            packages,
            // TODO: Get modules back from disk by grabbing the manifest or something, we probably
            // want to know a module came from us
            modules: HashSet::new(),
            // TODO: We don't *need* to keep track of dir/file/contents/symlinks
            // (if the project already exists, we can assume we aren't responsible for cleaning
            // up files or dirs from disk or anything, and they already exist so we don't ened to
            // configure them. That said, we *could* and it might be helpful)
            directories: HashMap::new(),
            files: HashMap::new(),
            file_contents: HashMap::new(),
            path_symlinks: HashMap::new(),
        })
    }
}

impl Project {
    fn setup_project(&self) -> Result<()> {
        if !self.path.path.is_dir() {
            debug!(
                "Project path {} does not exist. Creating it.",
                self.path.path.display()
            );
            create_dir_all(&self.path.path)?;
        }

        let (project_setup, extra_args) = if self.base.path.join(".project-properties").is_dir() {
            (self.path.path.join("bin").join("project-setup"), vec![])
        } else {
            (
                self.base.path.join("bin").join("project-setup"),
                vec!["--ignore-existing-files", "--with-gmake", "--with-cmake"],
            )
        };

        ensure!(
            project_setup.exists(),
            "Could not find `project-setup` binary in '{}'",
            self.base.path.display()
        );

        info!("Setting up project at {}", self.path.path.display());

        let output = Command::new(&project_setup)
            .args(&extra_args)
            .arg(&self.path.path)
            .current_dir(&self.path.path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;

        output.status.success().then_some(()).ok_or_else(|| {
            anyhow!(
                "Failed to run {}:\nstdout: {}\nstderr: {}",
                project_setup.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn setup_project_directories(&self) -> Result<()> {
        self.directories.iter().try_for_each(|(src, dst)| {
            debug!("Adding directory {} to {:?}", src.display(), dst);
            dst.canonicalize(&self.path.path)
                .map_err(|e| {
                    anyhow!(
                        "Failed to canonicalize project path {}: {}",
                        self.path.path.display(),
                        e
                    )
                })
                .and_then(|dst| copy_dir_contents(src, &dst))
        })
    }

    fn setup_project_files(&self) -> Result<()> {
        self.files.iter().try_for_each(|(src, dst)| {
            debug!("Adding file {} to {:?}", src.display(), dst);
            dst.canonicalize(&self.path.path).and_then(|dst| {
                dst.parent()
                    .ok_or_else(|| {
                        anyhow!("No parent directory of destination path {}", dst.display())
                    })
                    .and_then(|p| {
                        create_dir_all(p).map_err(|e| {
                            anyhow!("Couldn't create directory {}: {}", p.display(), e)
                        })
                    })
                    .and_then(|_| {
                        copy(src, &dst).map_err(|e| {
                            anyhow!("Couldn't copy {} to {:?}: {}", src.display(), dst, e)
                        })
                    })
                    .map(|_| ())
            })
        })
    }

    fn setup_project_file_contents(&self) -> Result<()> {
        self.file_contents.iter().try_for_each(|(contents, dst)| {
            debug!("Adding contents to {:?}", dst);
            dst.canonicalize(&self.path.path).and_then(|dst| {
                dst.parent()
                    .ok_or_else(|| {
                        anyhow!("No parent directory of destination path {}", dst.display())
                    })
                    .and_then(|p| {
                        debug!("Creating directory {}", p.display());
                        create_dir_all(p).map_err(|e| {
                            anyhow!("Couldn't create directory {}: {}", p.display(), e)
                        })
                    })
                    .and_then(|_| {
                        debug!("Writing file {}", dst.display());
                        OpenOptions::new()
                            .write(true)
                            .truncate(true)
                            .create(true)
                            .open(&dst)
                            .map_err(|e| anyhow!("Couldn't open file {}: {}", dst.display(), e))
                            .and_then(|mut f| {
                                f.write_all(contents).map_err(|e| {
                                    anyhow!("Couldn't write to file {}: {}", dst.display(), e)
                                })
                            })
                    })
            })
        })
    }

    fn setup_project_symlinks(&self) -> Result<()> {
        self.path_symlinks.iter().try_for_each(|(src, dst)| {
            debug!("Adding symlink from {} to {:?}", src.display(), dst);
            dst.canonicalize(&self.path.path).and_then(|dst| {
                dst.parent()
                    .ok_or_else(|| {
                        anyhow!("No parent directory of destination path {}", dst.display())
                    })
                    .and_then(|p| {
                        create_dir_all(p).map_err(|e| {
                            anyhow!("Couldn't create directory {}: {}", p.display(), e)
                        })
                    })
                    .and_then(|_| {
                        symlink(src, &dst).map_err(|e| {
                            anyhow!(
                                "Couldn't create symlink from {} to {}: {}",
                                src.display(),
                                dst.display(),
                                e
                            )
                        })
                    })
            })
        })
    }

    fn setup_project_contents(&self) -> Result<()> {
        self.setup_project_directories()?;
        self.setup_project_files()?;
        self.setup_project_file_contents()?;
        self.setup_project_symlinks()?;
        Ok(())
    }

    fn setup_packages(&self) -> Result<()> {
        let mut packages = read_to_string(self.path.path.join(".package-list"))
            .or_else(|e| {
                matches!(e.kind(), ErrorKind::NotFound)
                    .then(String::new)
                    .ok_or_else(|| anyhow!("Couldn't read existing .package-list file: {}", e))
            })?
            .lines()
            .map(|l| Package::try_from(PathBuf::from(l)))
            .collect::<Result<HashSet<_>>>()?;
        packages.extend(self.packages.iter().cloned());
        let packages_string = packages
            .iter()
            .map(|p| {
                info!("Adding package {}", p.name);
                p.path.to_string_lossy().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.path.path.join(".package-list"))
            .map_err(|e| {
                anyhow!(
                    "Couldn't open file {}: {}",
                    self.path.path.join(".package-list").display(),
                    e
                )
            })
            .and_then(|mut f| {
                f.write_all(packages_string.as_bytes())
                    .map_err(|e| anyhow!("Couldn't write packages list: {}", e))
                    .map(|_| ())
            })?;

        let project_setup = self.path.path.join("bin").join("project-setup");

        ensure!(
            project_setup.exists(),
            "Could not find `project-setup` binary in '{}'",
            self.base.path.display()
        );

        let output = Command::new(&project_setup)
            .arg(&self.path.path)
            .current_dir(&self.path.path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;

        output.status.success().then_some(()).ok_or_else(|| {
            anyhow!(
                "Failed to run {}:\nstdout: {}\nstderr: {}",
                project_setup.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn setup_modules(&self) -> Result<()> {
        self.modules.iter().try_for_each(|m| {
            info!("Adding module {}", m.artifact.package.name);
            m.setup(self).map(|_| ())
        })
    }

    pub fn setup(self) -> Result<Self> {
        self.setup_project()?;
        self.setup_project_contents()?;
        self.setup_packages()?;
        self.setup_modules()?;
        Ok(self)
    }
}

impl ProjectBuilder {
    /// Create a new project in a temporary directory. The directory will actually be
    /// created, because to securely create a tmpdir we need to hold it until we use it
    /// once we choose a name.
    fn default_path(&self) -> Result<ProjectPath> {
        ProjectPath::default()
    }

    /// The default version constraint is `==SIMICS_VERSION`
    fn default_base(&self) -> Result<Package> {
        let constraint: VersionConstraint = SIMICS_VERSION.parse()?;
        PackageBuilder::default()
            .package_number(PublicPackageNumber::Base)
            .version(constraint)
            .home(self.home.as_ref().cloned().unwrap_or(self.default_home()?))
            .build()
    }

    fn default_home(&self) -> Result<PathBuf> {
        simics_home()
    }
}

impl From<Project> for ProjectBuilder {
    fn from(value: Project) -> Self {
        Self {
            path: Some(value.path.clone()),
            base: Some(value.base.clone()),
            home: Some(value.home.clone()),
            packages: Some(value.packages.clone()),
            modules: Some(value.modules.clone()),
            directories: Some(value.directories.clone()),
            files: Some(value.files.clone()),
            file_contents: Some(value.file_contents.clone()),
            path_symlinks: Some(value.path_symlinks.clone()),
        }
    }
}

impl Debug for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Project")
            .field("path", &self.path)
            .field("base", &self.base)
            .field("home", &self.home)
            .field("packages", &self.packages)
            .field("modules", &self.modules)
            .field("directories", &self.directories)
            .field("files", &self.files)
            .field("file_contents", &self.file_contents.values())
            .field("path_symlinks", &self.path_symlinks)
            .finish()
    }
}

impl Project {
    pub fn cleanup(&mut self) {
        if self.path.temporary {
            remove_dir_all(&self.path.path)
                .map_err(|e| {
                    error!(
                        "Failed to remove temporary project from {}: {}",
                        self.path.path.display(),
                        e
                    );
                    e
                })
                .ok();
        }
    }
}

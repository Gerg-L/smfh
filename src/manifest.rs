use crate::{
    VERSION,
    file_util::{
        FileWithMetadata,
        prefix_move,
    },
};
use color_eyre::{
    Result,
    eyre::{
        Context as _,
        OptionExt as _,
    },
};
use core::{
    cmp::Ordering,
    fmt::{
        self,
    },
};
use log::{
    error,
    info,
    warn,
};
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
    de::Error as serdeErr,
};
use serde_json::Value;
use std::{
    fs::{
        self,
    },
    io::BufReader,
    path::{
        Component,
        Path,
        PathBuf,
    },
    process,
};

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<File>,
    pub clobber_by_default: bool,
    pub version: u16,
}

fn deserialize_octal<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let deserialized_value = Option::<String>::deserialize(deserializer)?;
    let Some(value) = deserialized_value else {
        // Don't error here because it's null!
        return Ok(None);
    };
    let x = u32::from_str_radix(&value, 8).map_err(serdeErr::custom)?;
    Ok(Some(x))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub source: Option<PathBuf>,
    pub target: PathBuf,
    #[serde(rename = "type")]
    pub kind: FileKind,
    pub clobber: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_octal")]
    pub permissions: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub deactivate: Option<bool>,
}

impl Ord for File {
    fn cmp(&self, other: &Self) -> Ordering {
        const fn value(file: &File) -> u8 {
            match file.kind {
                FileKind::Directory => 1,
                FileKind::Copy => 2,
                FileKind::Symlink => 3,
                FileKind::Modify => 4,
                FileKind::Delete => 5,
            }
        }

        if other.kind == self.kind {
            fn parents(path: &Path) -> usize {
                path.ancestors().count()
            }
            parents(&self.target).cmp(&parents(&other.target))
        } else {
            value(self).cmp(&value(other))
        }
    }
}

impl PartialOrd for File {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum FileKind {
    Directory,
    Copy,
    Symlink,
    Modify,
    Delete,
}
impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = match *self {
            Self::Copy => "copy",
            Self::Delete => "delete",
            Self::Directory => "directory",
            Self::Modify => "modify",
            Self::Symlink => "symlink",
        };
        write!(f, "{name}")
    }
}

impl Manifest {
    pub fn read(manifest_path: &Path) -> Self {
        let mut manifest = (move || -> Result<Self> {
            let file = fs::File::open(manifest_path).context("Failed to open manifest")?;
            let root: Value = serde_json::from_reader(BufReader::new(&file))
                .context("Failed to deserialize manifest")?;
            let version = root
                .get("version")
                .ok_or_eyre("Failed to get version from manifest")?;

            if version != VERSION {
                error!("Program version: '{VERSION}' Manifest version: '{version}'\n Version mismatch, exiting!");
                process::exit(2)
            }

            let deserialized_manifest: Self =
                serde_json::from_value(root).context("Failed to deserialize manifest")?;

            info!("Deserialized manifest: '{}'", manifest_path.display());
            Ok(deserialized_manifest)
        })()
        .unwrap_or_else(|err| {
            error!("{err}");
            process::exit(3)
        });

        if !cfg!(debug_assertions) {
            manifest.files.retain(|file| {
                let absolute = file.target.is_absolute()
                    && !file.target.components().any(|x| x == Component::ParentDir)
                    && file.source.as_ref().is_none_or(|x| x.is_absolute());
                if !absolute {
                    warn!(
                        "{} with target '{}' is not absolute, ignoring.",
                        file.kind,
                        file.target.display()
                    );
                }
                absolute
            });
        }

        manifest
    }
    pub fn activate(&mut self, prefix: &str) {
        self.files.sort();
        for mut file in self.files.iter().map(FileWithMetadata::from) {
            _ = file
                .activate(self.clobber_by_default, prefix)
                .inspect_err(|err| {
                    error!(
                        "Failed to activate file: '{}'\n Reason: '{}'",
                        file.target.display(),
                        err
                    );
                });
        }
    }

    pub fn deactivate(&mut self) {
        self.files.sort();
        for mut file in self.files.iter().map(FileWithMetadata::from).rev() {
            _ = file.deactivate().inspect_err(|err| {
                error!(
                    "Failed to deactivate file: '{}'\n Reason: '{}'",
                    file.target.display(),
                    err
                );
            });
        }
    }

    pub fn diff(mut self, mut old_manifest: Self, prefix: &str) {
        let mut updated_files: Vec<(File, File)> = vec![];
        let mut same_files: Vec<File> = vec![];

        old_manifest.files.retain(|file| {
            if let Some(index) = self.files.iter().position(|inner| inner == file) {
                same_files.push(self.files.swap_remove(index));
                false
            } else if let Some(index) = self.files.iter().position(|inner| {
                matches!(inner.clone(), File {
                    kind: FileKind::Symlink | FileKind::Copy,
                   target,
                    ..
                } if (target == file.target))
            }) {
                updated_files.push((file.clone(), self.files.swap_remove(index)));
                false
            } else {
                true
            }
        });

        // Remove files in old manifest
        // which aren't in new manifest
        old_manifest.deactivate();

        for (old, new) in updated_files {
            if !old.clobber.unwrap_or(old_manifest.clobber_by_default) {
                let mut file = FileWithMetadata::from(&old);

                // Don't care if this errors
                // metadata will just be none
                _ = file.set_metadata();

                if file.metadata.is_some()
                    && !file
                        .check()
                        .inspect_err(|err| warn!("Failed to check file: '{}', assuming file is incorrect\nReason: {}", file.target.display(), err))
                        .unwrap_or(false)
                {
                 if let Err(err) = prefix_move(&file.target, prefix) {
                     warn!("Failed to backup file '{}'\nReason: {}", file.target.display(), err);
                 }
                // if file existed but was wrong,
                // atomic action cannot be taken
                // so there's no point of forcing clobber

                // except this double checks
                 self.files.push(new.clone());
                 continue;
                }
            }

            let atomic = FileWithMetadata::from(&new.clone())
                .atomic_activate()
                .inspect_err(|err| {
                    error!(
                        "Failed to (atomic) activate file: '{}'\n Reason: '{}'",
                        new.target.display(),
                        err
                    );
                });
            if atomic.unwrap_or(false) {
                self.files.push(new);
            }
        }

        // These files could technically just be
        // Verified
        self.files.append(&mut same_files);
        // Activate new files
        self.activate(prefix);
    }
}

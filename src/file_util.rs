use crate::{
    file_util,
    manifest,
};
use blake3::Hash;
use color_eyre::{
    Result,
    eyre::{
        OptionExt,
        eyre,
    },
};
use log::{
    info,
    warn,
};
use manifest::{
    File,
    FileKind,
};
use std::{
    ffi::OsString,
    fs::{
        self,
        Metadata,
    },
    os::unix::fs::{
        MetadataExt,
        PermissionsExt,
        chown,
        lchown,
        symlink,
    },
    path::{
        Path,
        PathBuf,
    },
};
pub struct FileWithMetadata {
    pub source: Option<PathBuf>,
    pub target: PathBuf,
    pub kind: FileKind,
    pub clobber: Option<bool>,

    pub permissions: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub deactivate: Option<bool>,

    pub metadata: Option<Metadata>,
}

impl From<&File> for FileWithMetadata {
    fn from(f: &File) -> Self {
        FileWithMetadata {
            source: f.source.clone(),
            target: f.target.clone(),
            kind: f.kind,

            clobber: f.clobber,
            permissions: f.permissions,
            uid: f.uid,
            gid: f.gid,
            deactivate: f.deactivate,
            metadata: None,
        }
    }
}
impl FileWithMetadata {
    pub fn activate(&mut self, clobber_by_default: bool, prefix: &str) -> Result<()> {
        if self.missing_source() {
            return Ok(());
        }

        self.set_metadata()?;

        let clobber = self.clobber.unwrap_or(clobber_by_default);

        if clobber && self.metadata.is_some() && self.atomic_activate()? {
            return Ok(());
        };

        if self.check().unwrap_or(false) {
            info!("File '{}' already correct", self.target.display());
            return Ok(());
        }

        if match self {
            FileWithMetadata { metadata: None, .. }
            | FileWithMetadata {
                kind: FileKind::Modify | FileKind::Delete,
                ..
            } => false,
            // Don't clobber directories
            // If they're supposed to be
            // directories
            FileWithMetadata {
                kind: FileKind::Directory,
                metadata: Some(metadata),
                ..
            } => !metadata.is_dir(),
            _ => true,
        } {
            if clobber {
                delete(&self.target, self.metadata.as_ref().unwrap())?;
            } else {
                prefix_move(&self.target, prefix)?;
            }
        }

        match self.kind {
            FileKind::Directory => self.directory(),
            FileKind::File => self.copy(),
            FileKind::Symlink => self.symlink(),
            FileKind::Modify => self.chmod_chown(),
            FileKind::Delete => delete(&self.target, self.metadata.as_ref().unwrap()),
        }
    }

    pub fn atomic_activate(&mut self) -> Result<bool> {
        match self.kind {
            FileKind::Symlink | FileKind::File => {
                let target_is_dir = self.metadata.as_ref().unwrap().is_dir();
                let target_is_empty = self.source.as_ref().unwrap().read_dir()?.next().is_none();

                let source_is_dir = fs::symlink_metadata(self.source.as_ref().unwrap())?.is_dir();

                if target_is_dir != source_is_dir
                    || target_is_dir && source_is_dir && !target_is_empty
                {
                    return Ok(false);
                };

                let target = self.target.clone();

                self.target.set_extension("smfh-temp");
                match self.kind {
                    FileKind::Symlink => self.symlink(),
                    FileKind::File => self.copy(),
                    _ => panic!("This should never happen"),
                }?;
                info!(
                    "Renaming '{}' -> '{}'",
                    &self.target.display(),
                    target.display()
                );
                fs::rename(&self.target, target)?;

                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn deactivate(&mut self) -> Result<()> {
        if !self.deactivate.unwrap_or(true) {
            return Ok(());
        }

        self.set_metadata()?;

        if self.metadata.is_none() {
            info!("File already deleted '{}'", self.target.display());
            return Ok(());
        }

        if !self.check()? {
            return Err(eyre!("File is not the same as expected"));
        }

        match self.kind {
            // no-op on deactivation
            FileKind::Delete | FileKind::Modify => Ok(()),
            // delete only if directory is empty
            FileKind::Directory => rmdir(&self.target),
            // delete only if types match
            FileKind::Symlink | FileKind::File => {
                delete(&self.target, self.metadata.as_ref().unwrap())
            }
        }
    }

    pub fn check(&self) -> Result<bool> {
        match *self {
            FileWithMetadata {
                metadata: None,
                kind,
                ..
            } => Ok(kind == FileKind::Delete),
            FileWithMetadata {
                metadata: Some(_),
                kind: FileKind::Delete,
                ..
            } => Ok(false),
            // This should never happen
            // as it's checked before this
            // function is ever called
            FileWithMetadata {
                source: None,
                kind: FileKind::Symlink | FileKind::File,
                ref target,
                ..
            } => Err(eyre!("File '{}' missing_source", target.display())),
            FileWithMetadata {
                kind: FileKind::File | FileKind::Directory | FileKind::Modify,
                permissions: Some(perms),
                metadata: Some(ref metadata),
                ..
            }

            if perms != (metadata.mode() & 0o777) => Ok(false),
            FileWithMetadata {
                uid: Some(uid),
                metadata: Some(ref metadata),
                ..
            } if uid != metadata.uid() => Ok(false),
            FileWithMetadata {
                gid: Some(gid),
                metadata: Some(ref metadata),
                ..
            } if gid != metadata.gid() => Ok(false),

            FileWithMetadata {
                kind: FileKind::Symlink,
                ref target,
                source: Some(ref source),
                ..
                    // This will fail if target
                    // is a dead symlink
                    // which should only happen
                    // if source does not exist
                    // which should never happen
            } => Ok(fs::canonicalize(target)? == fs::canonicalize(source)?),
            FileWithMetadata {
                kind: FileKind::Directory,
                metadata: Some(ref metadata),
                ..
            } => Ok(metadata.is_dir()),
            FileWithMetadata {
                kind: FileKind::File,
                metadata: Some(ref metadata),
                ..
            } if !metadata.is_file() => Ok(false),
            FileWithMetadata {
                kind: FileKind::File,
                ref target,
                source: Some(ref source),
                ..
            } => {
                if fs::symlink_metadata(target)?.len() != fs::symlink_metadata(source)?.len() {
                    return Ok(false)
                };

                match (hash_file(target), hash_file(source)) {
                        (Some(l), Some(r)) => Ok(l == r),
                        _ => Ok(false)
                    }
            }
            FileWithMetadata {
                kind: FileKind::Modify,
                ..
            } => Ok(true),
        }
    }

    pub fn set_metadata(&mut self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.target);
        if let Err(ref e) = metadata {
            if e.kind() == std::io::ErrorKind::NotFound {
                self.metadata = None;
                return Ok(());
            };
        }
        self.metadata = Some(metadata?);
        Ok(())
    }
    pub fn missing_source(&self) -> bool {
        match *self {
            FileWithMetadata {
                source: None,
                kind: FileKind::File | FileKind::Symlink,
                ..
            } => {
                warn!(
                    "File '{}' missing source, skipping...",
                    self.target.display()
                );
                true
            }
            _ => false,
        }
    }

    pub fn chmod_chown(&mut self) -> Result<()> {
        self.set_metadata()?;
        let Some(metadata) = self.metadata.clone() else {
            return Err(eyre!(
                "Can't modify file '{}', file does not exist",
                self.target.display()
            ));
        };

        if self.kind != FileKind::Symlink {
            if let Some(x) = self.permissions {
                let new_perms = fs::Permissions::from_mode(x);

                if metadata.mode() & 0o777 == new_perms.mode() {
                    return Ok(());
                };
                info!(
                    "Setting permissions of: '{}' to: '{:o}'",
                    &self.target.display(),
                    new_perms.mode(),
                );

                //This doesn't work with symlinks
                fs::set_permissions(&self.target, new_perms)?;
            }
            self.set_metadata()?;
        }

        if self.uid.is_some() || self.uid.is_some() {
            if (self.uid.is_some_and(|x| x == metadata.uid()))
                && (self.gid.is_some_and(|x| x == metadata.gid()))
            {
                return Ok(());
            }
            info!(
                "Chowning '{}': 'uid:{} gid:{}' -> 'uid:{} gid::{}'",
                self.target.display(),
                metadata.uid(),
                metadata.gid(),
                self.uid.unwrap_or(metadata.uid()),
                self.gid.unwrap_or(metadata.gid()),
            );
            if metadata.is_symlink() {
                lchown(&self.target, self.uid, self.gid)?;
            } else {
                chown(&self.target, self.uid, self.gid)?;
            };
        }
        Ok(())
    }

    pub fn symlink(&mut self) -> Result<()> {
        let _ = file_util::mkdir(
            self.target
                .parent()
                .ok_or_eyre("Failed to get parent directory")?,
        );
        let source = fs::canonicalize(self.source.as_ref().unwrap())?;
        symlink(&source, &self.target)?;
        info!(
            "Symlinked '{}' -> '{}'",
            source.display(),
            &self.target.display(),
        );

        self.set_metadata()?;
        self.chmod_chown()?;
        Ok(())
    }

    pub fn copy(&mut self) -> Result<()> {
        let _ = file_util::mkdir(
            self.target
                .parent()
                .ok_or_eyre("Failed to get parent directory")?,
        );
        let source = fs::canonicalize(self.source.as_ref().unwrap())?;
        fs::copy(&source, &self.target)?;
        info!(
            "Copied '{}' -> '{}'",
            source.display(),
            &self.target.display(),
        );

        self.set_metadata()?;
        self.chmod_chown()?;
        Ok(())
    }

    pub fn directory(&mut self) -> Result<()> {
        mkdir(&self.target)?;
        self.set_metadata()?;
        self.chmod_chown()?;
        Ok(())
    }
}
pub fn mkdir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(_) => {
            fs::create_dir_all(path)?;
            info!("Created directory '{}'", path.display());
        }
        Ok(x) => {
            if !x.is_dir() {
                return Err(eyre!("File in way of '{}'", path.display()));
            };
            info!("Directory '{}' already exists", path.display());
        }
    };
    Ok(())
}

pub fn prefix_move(path: &Path, prefix: &str) -> Result<()> {
    let Ok(_) = fs::symlink_metadata(path) else {
        return Ok(());
    };

    let canon_path = fs::canonicalize(path)?;

    let mut appended_path = OsString::from(prefix);
    appended_path.push(canon_path.file_name().ok_or_eyre(format!(
        "Failed to get file name of file '{}'",
        path.display()
    ))?);

    let new_path = canon_path
        .parent()
        .ok_or_eyre(format!("Failed to get parent of file '{}'", path.display()))?
        .join(PathBuf::from(appended_path));

    if fs::symlink_metadata(&new_path).is_ok() {
        prefix_move(&new_path, prefix)?;
    };

    fs::rename(canon_path, &new_path)?;
    info!("Renaming '{}' -> '{}'", path.display(), new_path.display());
    Ok(())
}

pub fn rmdir(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.is_dir() {
        return Err(eyre!("Path '{}' is not a directory", path.display()));
    }
    fs::remove_dir(path)?;
    info!("Deleting directory '{}'", path.display());
    Ok(())
}

pub fn hash_file(filepath: &Path) -> Option<Hash> {
    let mut hasher = blake3::Hasher::new();

    if let Err(e) = hasher.update_mmap(filepath) {
        warn!(
            "Failed to hash file: '{}'\nReason: '{}'",
            filepath.display(),
            e
        );
        return None;
    };
    Some(hasher.finalize())
}

pub fn delete(filepath: &Path, metadata: &Metadata) -> Result<()> {
    if metadata.is_dir() {
        fs::remove_dir_all(filepath)?;
    } else {
        fs::remove_file(filepath)?;
    }
    info!("Deleted '{}'", filepath.display());
    Ok(())
}

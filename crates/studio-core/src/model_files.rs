//! Removal of a prevalidated model directory through anchored Unix descriptors.
//! Scope, ownership and shared-cache policies live in `models`; this module never
//! resolves an absolute path again after opening and checking the two descriptors.

use anyhow::{Context, Result, bail};
use std::{
    ffi::{CStr, CString},
    fs::{self, File, Metadata},
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Path,
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn stat(stat: &libc::stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        }
    }
}

pub(crate) struct ModelDirectory {
    parent: File,
    root: File,
    name: CString,
    identity: Identity,
}

impl ModelDirectory {
    pub(crate) fn open(
        path: &Path,
        expected_root: &Metadata,
        expected_parent: &Metadata,
    ) -> Result<Self> {
        if !path.is_absolute()
            || path
                .canonicalize()
                .context("Model directory is unavailable")?
                != path
        {
            bail!("The model directory changed or is not an absolute canonical path");
        }
        if !expected_root.file_type().is_dir() || !expected_parent.file_type().is_dir() {
            bail!("The model and its parent must be real directories, not symbolic links");
        }
        let parent_path = path.parent().context("Cannot delete a filesystem root")?;
        let name = CString::new(
            path.file_name()
                .context("Cannot delete a filesystem root")?
                .as_bytes(),
        )?;
        let current_root = fs::symlink_metadata(path).context("Model directory is unavailable")?;
        let current_parent =
            fs::symlink_metadata(parent_path).context("Model parent directory is unavailable")?;
        if !current_root.file_type().is_dir()
            || !current_parent.file_type().is_dir()
            || Identity::metadata(&current_root) != Identity::metadata(expected_root)
            || Identity::metadata(&current_parent) != Identity::metadata(expected_parent)
        {
            bail!("The model directory or its parent changed during validation");
        }
        let parent_name = CString::new(parent_path.as_os_str().as_bytes())?;
        let parent = open_directory(libc::AT_FDCWD, &parent_name)
            .context("Could not open the model parent directory safely")?;
        if Identity::metadata(&parent.metadata()?) != Identity::metadata(expected_parent) {
            bail!("The model parent directory changed during validation");
        }
        let root = open_directory(parent.as_raw_fd(), &name)
            .context("Could not open the model directory safely")?;
        let identity = Identity::metadata(&root.metadata()?);
        if identity != Identity::metadata(expected_root) {
            bail!("The model directory changed during validation");
        }
        if identity.device != Identity::metadata(expected_parent).device {
            bail!("Cannot purge a model directory that is a filesystem mount point");
        }
        Ok(Self {
            parent,
            root,
            name,
            identity,
        })
    }

    #[cfg(test)]
    fn metadata(&self) -> Result<Metadata> {
        self.root
            .metadata()
            .context("Could not inspect the opened model directory")
    }

    pub(crate) fn remove(self) -> Result<()> {
        // A renamed/replaced entry must fail before any of its old contents are
        // touched. Recursion and final unlink stay relative to the opened parent.
        verify_entry(self.parent.as_raw_fd(), &self.name, self.identity)?;
        remove_contents(&self.root, self.identity.device)?;
        verify_entry(self.parent.as_raw_fd(), &self.name, self.identity)?;
        unlink(self.parent.as_raw_fd(), &self.name, libc::AT_REMOVEDIR)
            .context("Could not remove the emptied model directory")
    }
}

fn open_directory(parent: RawFd, name: &CStr) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn stat_entry(parent: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { stat.assume_init() })
    }
}

fn verify_entry(parent: RawFd, name: &CStr, expected: Identity) -> Result<()> {
    let stat =
        stat_entry(parent, name).context("A model directory entry changed during removal")?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR || Identity::stat(&stat) != expected {
        bail!("A model directory entry changed during removal");
    }
    Ok(())
}

fn unlink(parent: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    if unsafe { libc::unlinkat(parent, name.as_ptr(), flags) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn remove_contents(directory: &File, device: u64) -> Result<()> {
    for name in directory_names(directory)? {
        let stat = stat_entry(directory.as_raw_fd(), &name)
            .context("Could not inspect a model file during removal")?;
        let identity = Identity::stat(&stat);
        if identity.device != device {
            bail!("Cannot purge a model containing a filesystem mount point");
        }
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = open_directory(directory.as_raw_fd(), &name)
                .context("A model subdirectory changed during removal")?;
            if Identity::metadata(&child.metadata()?) != identity {
                bail!("A model subdirectory changed during removal");
            }
            remove_contents(&child, device)?;
            verify_entry(directory.as_raw_fd(), &name, identity)?;
            unlink(directory.as_raw_fd(), &name, libc::AT_REMOVEDIR)
                .context("Could not remove a model subdirectory")?;
        } else {
            // In particular, a symbolic link is unlinked, never opened or followed.
            unlink(directory.as_raw_fd(), &name, 0).context("Could not remove a model file")?;
        }
    }
    Ok(())
}

struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn directory_names(directory: &File) -> Result<Vec<CString>> {
    let duplicate = directory
        .try_clone()
        .context("Could not duplicate the model directory descriptor")?;
    let raw = duplicate.into_raw_fd();
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        // fdopendir takes ownership only on success.
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(raw);
        }
        return Err(error).context("Could not enumerate the model directory");
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        clear_errno();
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(0) {
                break;
            }
            return Err(error).context("Could not enumerate the model directory");
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly"
))]
fn clear_errno() {
    unsafe {
        *libc::__error() = 0;
    }
}
#[cfg(any(target_os = "linux", target_os = "hurd"))]
fn clear_errno() {
    unsafe {
        *libc::__errno_location() = 0;
    }
}
#[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
fn clear_errno() {
    unsafe {
        *libc::__errno() = 0;
    }
}
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
fn clear_errno() {
    unsafe {
        *libc::___errno() = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn open_model(path: &Path) -> ModelDirectory {
        ModelDirectory::open(
            path,
            &fs::symlink_metadata(path).unwrap(),
            &fs::symlink_metadata(path.parent().unwrap()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn removes_only_the_anchored_tree_without_following_child_links() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().canonicalize().unwrap();
        let model = parent.join("model");
        let outside = parent.join("outside");
        fs::create_dir_all(model.join("nested")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(model.join("weights.safetensors"), b"model weights").unwrap();
        fs::write(model.join("nested/config.json"), b"{}").unwrap();
        fs::write(outside.join("keep"), b"unrelated model").unwrap();
        symlink(&outside, model.join("linked-directory")).unwrap();
        symlink(outside.join("keep"), model.join("linked-file")).unwrap();
        let opened = open_model(&model);
        assert!(opened.metadata().unwrap().is_dir());
        opened.remove().unwrap();
        assert!(!model.exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"unrelated model");
    }

    #[test]
    fn refuses_a_root_replaced_after_open_without_touching_either_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().canonicalize().unwrap();
        let model = parent.join("model");
        let original = parent.join("original");
        fs::create_dir(&model).unwrap();
        fs::write(model.join("keep-original"), b"original").unwrap();
        let opened = open_model(&model);
        fs::rename(&model, &original).unwrap();
        fs::create_dir(&model).unwrap();
        fs::write(model.join("keep-replacement"), b"replacement").unwrap();
        assert!(opened.remove().is_err());
        assert_eq!(
            fs::read(original.join("keep-original")).unwrap(),
            b"original"
        );
        assert_eq!(
            fs::read(model.join("keep-replacement")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn refuses_an_identity_changed_between_prevalidation_and_open() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().canonicalize().unwrap();
        let model = parent.join("model");
        fs::create_dir(&model).unwrap();
        let root_metadata = fs::symlink_metadata(&model).unwrap();
        let parent_metadata = fs::symlink_metadata(&parent).unwrap();
        fs::rename(&model, parent.join("original")).unwrap();
        fs::create_dir(&model).unwrap();
        assert!(ModelDirectory::open(&model, &root_metadata, &parent_metadata).is_err());
    }

    #[test]
    fn refuses_a_symbolic_link_root_and_a_changed_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let temporary = temporary.path().canonicalize().unwrap();
        let parent = temporary.join("parent");
        let model = parent.join("model");
        fs::create_dir_all(&model).unwrap();
        let root_metadata = fs::symlink_metadata(&model).unwrap();
        let parent_metadata = fs::symlink_metadata(&parent).unwrap();
        let link = temporary.join("link");
        symlink(&model, &link).unwrap();
        assert!(
            ModelDirectory::open(
                &link,
                &fs::symlink_metadata(&link).unwrap(),
                &fs::symlink_metadata(&temporary).unwrap()
            )
            .is_err()
        );
        fs::rename(&parent, temporary.join("original-parent")).unwrap();
        fs::create_dir_all(&model).unwrap();
        assert!(ModelDirectory::open(&model, &root_metadata, &parent_metadata).is_err());
    }
}

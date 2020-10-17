//! The core module for file management.
#![no_std]
#![feature(new_uninit)]
#![feature(get_mut_unchecked)]
#![feature(negative_impls)]
#![feature(try_reserve)]

#[cfg(test)]
extern crate std;

extern crate alloc;
extern crate core;
extern crate hashbrown;
#[macro_use]
extern crate static_assertions;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use custom_error::custom_error;
use hashbrown::HashMap;
pub use io::*;
use mnode::{MemNode, NodeType};
use rwlock::RwLock as NrLock;
use spin::RwLock;

mod fd;
mod file;
pub mod io;
mod mnode;
mod rwlock;
mod topology;

/// The maximum number of open files for a process.
pub const MAX_FILES_PER_PROCESS: usize = 1024;

/// Mnode number.
pub type Mnode = u64;
/// Flags for fs calls.
pub type Flags = u64;
/// Modes for fs calls
pub type Modes = u64;
/// File descriptor.
pub type FD = u64;
/// Userspace buffer pointer to read or write a file.
pub type Buffer = u64;
/// Number of bytes to read or write a file.
pub type Len = u64;
/// Userspace-pointer to filename.
pub type Filename = u64;
/// File offset
pub type Offset = i64;

custom_error! {
    #[derive(PartialEq, Clone)]
    pub FileSystemError
    InvalidFileDescriptor = "Supplied file descriptor was invalid",
    InvalidFile = "Supplied file was invalid",
    InvalidFlags = "Supplied flags were invalid",
    InvalidOffset = "Supplied offset was invalid",
    PermissionError = "File/directory can't be read or written",
    AlreadyPresent = "Fd/File already exists",
    DirectoryError = "Can't read or write to a directory",
    OpenFileLimit = "Maximum files are opened for a process",
    OutOfMemory = "Unable to allocate memory for file",
}

/// Abstract definition of file-system interface operations.
pub trait FileSystem {
    fn create(&self, pathname: &str, modes: Modes) -> Result<Mnode, FileSystemError>;
    fn write(
        &self,
        mnode_num: Mnode,
        buffer: &[u8],
        offset: usize,
    ) -> Result<usize, FileSystemError>;
    fn read(
        &self,
        mnode_num: Mnode,
        buffer: &mut [u8],
        offset: usize,
    ) -> Result<usize, FileSystemError>;
    fn lookup(&self, pathname: &str) -> Option<Arc<Mnode>>;
    fn file_info(&self, mnode: Mnode) -> FileInfo;
    fn delete(&self, pathname: &str) -> Result<bool, FileSystemError>;
    fn truncate(&self, pathname: &str) -> Result<bool, FileSystemError>;
    fn rename(&self, oldname: &str, newname: &str) -> Result<bool, FileSystemError>;
}

/// The in-memory file-system representation.
//#[derive(Debug)]
pub struct MemFS {
    mnodes: NrLock<HashMap<Mnode, RwLock<MemNode>>>,
    files: RwLock<HashMap<String, Arc<Mnode>>>,
    _root: (String, Mnode),
    nextmemnode: AtomicUsize,
}

impl MemFS {
    /// Get the next available memnode number.
    fn get_next_mno(&self) -> usize {
        self.nextmemnode.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for MemFS {
    /// Initialize the file system from the root directory.
    fn default() -> MemFS {
        let rootdir = "/";
        let rootmnode = 1;

        let mnodes = NrLock::<HashMap<Mnode, RwLock<MemNode>>>::default();
        mnodes.write().insert(
            rootmnode,
            RwLock::new(
                MemNode::new(
                    rootmnode,
                    rootdir,
                    FileModes::S_IRWXU.into(),
                    NodeType::Directory,
                )
                .unwrap(),
            ),
        );
        let files = RwLock::new(HashMap::new());
        files.write().insert(rootdir.to_string(), Arc::new(1));
        let _root = (rootdir.to_string(), 1);

        MemFS {
            mnodes,
            files,
            _root,
            nextmemnode: AtomicUsize::new(2),
        }
    }
}

impl FileSystem for MemFS {
    /// Create a file relative to the root directory.
    fn create(&self, pathname: &str, modes: Modes) -> Result<Mnode, FileSystemError> {
        // Check if the file with the same name already exists.
        match self.files.read().get(&pathname.to_string()) {
            Some(_) => return Err(FileSystemError::AlreadyPresent),
            None => {}
        }

        let mnode_num = self.get_next_mno() as u64;
        //TODO: For now all newly created mnode are for file. How to differentiate
        // between a file and a directory. Take input from the user?
        let memnode = match MemNode::new(mnode_num, pathname, modes, NodeType::File) {
            Ok(memnode) => memnode,
            Err(e) => return Err(e),
        };
        self.files
            .write()
            .insert(pathname.to_string(), Arc::new(mnode_num));
        self.mnodes.write().insert(mnode_num, RwLock::new(memnode));

        Ok(mnode_num)
    }

    /// Write data to a file.
    fn write(
        &self,
        mnode_num: Mnode,
        buffer: &[u8],
        offset: usize,
    ) -> Result<usize, FileSystemError> {
        match self.mnodes.read(mnode_num as usize - 1).get(&mnode_num) {
            Some(mnode) => mnode.write().write(buffer, offset),
            None => Err(FileSystemError::InvalidFile),
        }
    }

    /// Read data from a file.
    fn read(
        &self,
        mnode_num: Mnode,
        buffer: &mut [u8],
        offset: usize,
    ) -> Result<usize, FileSystemError> {
        match self.mnodes.read(mnode_num as usize - 1).get(&mnode_num) {
            Some(mnode) => mnode.read().read(buffer, offset),
            None => Err(FileSystemError::InvalidFile),
        }
    }

    /// Check if a file exists in the file system or not.
    fn lookup(&self, pathname: &str) -> Option<Arc<Mnode>> {
        self.files
            .read()
            .get(&pathname.to_string())
            .map(|mnode| Arc::clone(mnode))
    }

    /// Find the size and type by giving the mnode number.
    fn file_info(&self, mnode: Mnode) -> FileInfo {
        match self.mnodes.read(mnode as usize - 1).get(&mnode) {
            Some(mnode) => match mnode.read().get_mnode_type() {
                NodeType::Directory => FileInfo {
                    fsize: 0,
                    ftype: NodeType::Directory.into(),
                },
                NodeType::File => FileInfo {
                    fsize: mnode.read().get_file_size() as u64,
                    ftype: NodeType::File.into(),
                },
            },
            None => unreachable!("file_info: shouldn't reach here"),
        }
    }

    /// Delete a file from the file-system.
    fn delete(&self, pathname: &str) -> Result<bool, FileSystemError> {
        match self.files.write().remove(&pathname.to_string()) {
            Some(mnode) => {
                // If the pathname is the only link to the memnode, then remove it.
                match Arc::strong_count(&mnode) {
                    1 => {
                        self.mnodes.write().remove(&mnode);
                        return Ok(true);
                    }
                    _ => {
                        self.files.write().insert(pathname.to_string(), mnode);
                        return Err(FileSystemError::PermissionError);
                    }
                }
            }
            None => return Err(FileSystemError::InvalidFile),
        };
    }

    fn truncate(&self, pathname: &str) -> Result<bool, FileSystemError> {
        match self.files.read().get(&pathname.to_string()) {
            Some(mnode) => match self.mnodes.read(0).get(mnode) {
                Some(memnode) => memnode.write().file_truncate(),
                None => return Err(FileSystemError::InvalidFile),
            },
            None => return Err(FileSystemError::InvalidFile),
        }
    }

    /// Rename a file from oldname to newname.
    fn rename(&self, oldname: &str, newname: &str) -> Result<bool, FileSystemError> {
        if self.files.read().get(oldname).is_none() {
            return Err(FileSystemError::InvalidFile);
        }

        // If the newfile exists then overwrite it with the oldfile.
        if self.files.read().get(newname).is_some() {
            self.delete(newname).unwrap();
        }

        let (_key, value) = self.files.write().remove_entry(oldname).unwrap();
        match self.files.write().insert(newname.to_string(), value) {
            None => return Ok(true),
            Some(_) => return Err(FileSystemError::PermissionError),
        }
    }
}

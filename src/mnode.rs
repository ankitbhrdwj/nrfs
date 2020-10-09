use alloc::string::String;
use alloc::string::ToString;

use crate::file::*;
use crate::{FileSystemError, Mnode, Modes};

/// Each memory-node can be of two types: directory or a file.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
#[repr(u64)]
pub enum NodeType {
    /// The mnode is of directory type
    Directory = 1,
    /// The mnode is of regular type
    File = 2,
}

impl Into<u64> for NodeType {
    fn into(self) -> u64 {
        match self {
            NodeType::Directory => 1,
            NodeType::File => 2,
        }
    }
}

/// Memnode representation, similar to Inode for a memory-fs.
#[derive(Debug)]
pub struct MemNode {
    mnode_num: Mnode,
    name: String,
    node_type: NodeType,
    file: Option<File>,
}

/// Required for the testing
impl PartialEq for MemNode {
    fn eq(&self, other: &Self) -> bool {
        (self.mnode_num == other.mnode_num)
            && (self.name == other.name)
            && (self.node_type == other.node_type)
            && (self.file == other.file)
    }
}

impl MemNode {
    /// Initialize a memory-node for a directory or a file.
    pub fn new(
        mnode_num: Mnode,
        pathname: &str,
        modes: Modes,
        node_type: NodeType,
    ) -> Result<MemNode, FileSystemError> {
        let file = match node_type {
            NodeType::Directory => None,
            NodeType::File => match File::new(modes) {
                Ok(file) => Some(file),
                Err(e) => return Err(e),
            },
        };

        Ok(MemNode {
            mnode_num,
            name: pathname.to_string(),
            node_type,
            file,
        })
    }

    /// Write to an in-memory file.
    pub fn write(&mut self, buffer: &[u8], offset: usize) -> Result<usize, FileSystemError> {
        // Return if the user doesn't have write permissions for the file.
        if self.node_type != NodeType::File || !self.file.as_ref().unwrap().get_mode().is_writable()
        {
            return Err(FileSystemError::PermissionError);
        }
        let len: usize = buffer.len();

        self.file.as_mut().unwrap().write_file(buffer, len, offset)
    }

    /// Read from an in-memory file.
    pub fn read(&self, buffer: &mut [u8], offset: usize) -> Result<usize, FileSystemError> {
        // Return if the user doesn't have read permissions for the file.
        if self.node_type != NodeType::File || !self.file.as_ref().unwrap().get_mode().is_readable()
        {
            return Err(FileSystemError::PermissionError);
        }

        let len: usize = buffer.len();
        let file_size = self.get_file_size();
        if offset > file_size {
            return Err(FileSystemError::InvalidOffset);
        }

        let bytes_to_read = core::cmp::min(file_size - offset, len);
        let new_offset = offset + bytes_to_read;

        // Return error if start-offset is greater than or equal to new-offset OR
        // new offset is greater than the file size.
        if offset >= new_offset || new_offset > self.get_file_size() as usize {
            return Err(FileSystemError::InvalidOffset);
        }

        // Read from file only if its not at EOF.
        match self
            .file
            .as_ref()
            .unwrap()
            .read_file(&mut *buffer, offset, new_offset)
        {
            Ok(len) => return Ok(len),
            Err(e) => return Err(e),
        }
    }

    /// Get the file size
    pub fn get_file_size(&self) -> usize {
        self.file.as_ref().unwrap().get_size()
    }

    /// Get the type of mnode; Directory or file.
    pub fn get_mnode_type(&self) -> NodeType {
        self.node_type
    }

    /// Truncate the file in reasponse of O_TRUNC flag.
    pub fn file_truncate(&mut self) -> Result<bool, FileSystemError> {
        if self.node_type != NodeType::File || !self.file.as_ref().unwrap().get_mode().is_writable()
        {
            return Err(FileSystemError::PermissionError);
        }

        // The method doesn't fail after this point, so returning Ok().
        self.file.as_mut().unwrap().file_truncate();
        Ok(true)
    }
}

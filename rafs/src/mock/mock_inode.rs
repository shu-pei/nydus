// Copyright 2020 Ant Group. All rights reserved.
// Copyright (C) 2020 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Result;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use fuse_backend_rs::abi::linux_abi;
use fuse_backend_rs::api::filesystem::Entry;

use storage::device::RafsBioDesc;

use crate::metadata::{
    layout::{
        v5::{
            rafsv5_alloc_bio_desc, RafsBlobEntry, RafsChunkInfo, RafsV5BlobTable, RafsV5Inode,
            RafsV5InodeFlags, RafsV5InodeOps,
        },
        XattrName, XattrValue,
    },
    Inode, RafsInode, RafsSuperMeta, RAFS_INODE_BLOCKSIZE,
};

use nydus_utils::{digest::RafsDigest, ByteSize};

use super::mock_chunk::MockChunkInfo;
use super::mock_super::CHUNK_SIZE;

#[derive(Default, Clone, Debug)]
pub struct MockInode {
    i_ino: Inode,
    i_name: OsString,
    i_digest: RafsDigest,
    i_parent: u64,
    i_mode: u32,
    i_projid: u32,
    i_uid: u32,
    i_gid: u32,
    i_flags: RafsV5InodeFlags,
    i_size: u64,
    i_blocks: u64,
    i_nlink: u32,
    i_child_idx: u32,
    i_child_cnt: u32,
    // extra info need cache
    i_blksize: u32,
    i_rdev: u32,
    i_mtime_nsec: u32,
    i_mtime: u64,
    i_target: OsString, // for symbol link
    i_xattr: HashMap<OsString, Vec<u8>>,
    i_data: Vec<Arc<MockChunkInfo>>,
    i_child: Vec<Arc<MockInode>>,
    i_blob_table: Arc<RafsV5BlobTable>,
    i_meta: Arc<RafsSuperMeta>,
}

impl MockInode {
    pub fn mock(ino: Inode, size: u64, chunks: Vec<Arc<MockChunkInfo>>) -> Self {
        Self {
            i_ino: ino,
            i_size: size,
            i_child_cnt: chunks.len() as u32,
            i_data: chunks,
            // Ignore other bits for now.
            i_mode: libc::S_IFREG,
            // It can't be changed yet.
            i_blksize: CHUNK_SIZE,
            ..Default::default()
        }
    }
}

impl RafsInode for MockInode {
    fn validate(&self) -> Result<()> {
        if self.is_symlink() && self.i_target.is_empty() {
            return Err(einval!("invalid inode"));
        }
        Ok(())
    }

    #[inline]
    fn get_entry(&self) -> Entry {
        Entry {
            attr: self.get_attr().into(),
            inode: self.i_ino,
            generation: 0,
            attr_timeout: self.i_meta.attr_timeout,
            entry_timeout: self.i_meta.entry_timeout,
        }
    }

    #[inline]
    fn get_attr(&self) -> linux_abi::Attr {
        linux_abi::Attr {
            ino: self.i_ino,
            size: self.i_size,
            blocks: self.i_blocks,
            mode: self.i_mode,
            nlink: self.i_nlink as u32,
            blksize: RAFS_INODE_BLOCKSIZE,
            rdev: self.i_rdev,
            ..Default::default()
        }
    }

    fn get_symlink(&self) -> Result<OsString> {
        if !self.is_symlink() {
            Err(einval!("inode is not a symlink"))
        } else {
            Ok(self.i_target.clone())
        }
    }

    fn get_child_by_name(&self, name: &OsStr) -> Result<Arc<dyn RafsInode>> {
        let idx = self
            .i_child
            .binary_search_by(|c| c.i_name.as_os_str().cmp(name))
            .map_err(|_| enoent!())?;
        Ok(self.i_child[idx].clone())
    }

    #[inline]
    fn get_child_by_index(&self, index: Inode) -> Result<Arc<dyn RafsInode>> {
        Ok(self.i_child[index as usize].clone())
    }

    fn get_child_index(&self) -> Result<u32> {
        Ok(self.i_child_idx)
    }

    #[inline]
    fn get_child_count(&self) -> u32 {
        self.i_child_cnt
    }

    #[inline]
    fn get_chunk_info(&self, idx: u32) -> Result<Arc<dyn RafsChunkInfo>> {
        Ok(self.i_data[idx as usize].clone())
    }

    fn has_xattr(&self) -> bool {
        self.i_flags.contains(RafsV5InodeFlags::XATTR)
    }

    #[inline]
    fn get_xattr(&self, name: &OsStr) -> Result<Option<XattrValue>> {
        Ok(self.i_xattr.get(name).cloned())
    }

    fn get_xattrs(&self) -> Result<Vec<XattrName>> {
        Ok(self
            .i_xattr
            .keys()
            .map(|k| k.as_bytes().to_vec())
            .collect::<Vec<XattrName>>())
    }

    fn is_dir(&self) -> bool {
        self.i_mode & libc::S_IFMT == libc::S_IFDIR
    }

    fn is_symlink(&self) -> bool {
        self.i_mode & libc::S_IFMT == libc::S_IFLNK
    }

    fn is_reg(&self) -> bool {
        self.i_mode & libc::S_IFMT == libc::S_IFREG
    }

    fn is_hardlink(&self) -> bool {
        !self.is_dir() && self.i_nlink > 1
    }

    fn name(&self) -> OsString {
        self.i_name.clone()
    }

    fn flags(&self) -> u64 {
        self.i_flags.bits()
    }

    fn get_digest(&self) -> RafsDigest {
        self.i_digest
    }

    fn collect_descendants_inodes(
        &self,
        descendants: &mut Vec<Arc<dyn RafsInode>>,
    ) -> Result<usize> {
        if !self.is_dir() {
            return Err(enotdir!());
        }

        let mut child_dirs: Vec<Arc<dyn RafsInode>> = Vec::new();

        for child_inode in &self.i_child {
            if child_inode.is_dir() {
                trace!("Got dir {:?}", child_inode.name());
                child_dirs.push(child_inode.clone());
            } else {
                if child_inode.is_empty_size() {
                    continue;
                }
                descendants.push(child_inode.clone());
            }
        }

        for d in child_dirs {
            d.collect_descendants_inodes(descendants)?;
        }

        Ok(0)
    }

    fn alloc_bio_desc(&self, offset: u64, size: usize, user_io: bool) -> Result<RafsBioDesc> {
        rafsv5_alloc_bio_desc(self, offset, size, user_io)
    }

    fn get_name_size(&self) -> u16 {
        self.i_name.byte_size() as u16
    }

    fn get_symlink_size(&self) -> u16 {
        if self.is_symlink() {
            self.i_target.byte_size() as u16
        } else {
            0
        }
    }

    impl_getter!(ino, i_ino, u64);
    impl_getter!(parent, i_parent, u64);
    impl_getter!(size, i_size, u64);
    impl_getter!(rdev, i_rdev, u32);
    impl_getter!(projid, i_projid, u32);
}

impl RafsV5InodeOps for MockInode {
    fn get_blob_by_index(&self, _idx: u32) -> Result<Arc<RafsBlobEntry>> {
        Ok(Arc::new(RafsBlobEntry::default()))
    }

    fn get_blocksize(&self) -> u32 {
        CHUNK_SIZE
    }

    fn has_hole(&self) -> bool {
        false
    }

    fn cast_ondisk(&self) -> Result<RafsV5Inode> {
        unimplemented!()
    }
}

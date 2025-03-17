// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs::Permissions;
use std::io::Write;
use std::ops::DerefMut;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use anyhow::Result;

use rafs::metadata::layout::v5::{
    RafsV5BlobTable, RafsV5ChunkInfo, RafsV5ExtBlobTable, RafsV5Inode, RafsV5InodeTable,
    RafsV5PrefetchTable, RafsV5SuperBlock, RafsV5XAttrsTable,
};
use rafs::metadata::RafsSuperFlags;
use rafs::{RafsIoRead, RafsIoReader};

pub(crate) struct RafsInspector {
    request_mode: bool,
    bootstrap: Arc<Mutex<RafsIoReader>>,
    layout_profile: RafsLayoutV5,
    rafs_meta: RafsMeta,
    cur_dir_index: u32,
    parent_indexes: Vec<u32>,
    inodes_table: RafsV5InodeTable,
    blobs_table: RafsV5BlobTable,
    extended_blobs_table: Option<RafsV5ExtBlobTable>,
}

/// | Superblock | inode table | prefetch table |inode + name + symlink pointer + xattr size + xattr pairs + chunk info
#[allow(dead_code)]
pub(crate) struct RafsLayoutV5 {
    super_block_offset: u32,
    super_block_size: u32,
    inode_size: u32,
    chunk_info_size: u32,
}

pub(crate) struct RafsMeta {
    inode_table_offset: u64,
    inode_table_entries: u32,
    prefetch_table_offset: u64,
    prefetch_table_entries: u32,
    blob_table_offset: u64,
    blob_table_size: u32,
    extended_blob_table_offset: u64,
    extended_blob_table_entries: u32,
}

impl From<&RafsV5SuperBlock> for RafsMeta {
    fn from(sb: &RafsV5SuperBlock) -> Self {
        Self {
            inode_table_offset: sb.inode_table_offset(),
            inode_table_entries: sb.inode_table_entries(),
            prefetch_table_offset: sb.prefetch_table_offset(),
            blob_table_offset: sb.blob_table_offset(),
            blob_table_size: sb.blob_table_size(),
            prefetch_table_entries: sb.prefetch_table_entries(),
            extended_blob_table_offset: sb.extended_blob_table_offset(),
            extended_blob_table_entries: sb.extended_blob_table_entries(),
        }
    }
}

impl RafsLayoutV5 {
    pub fn new() -> Self {
        RafsLayoutV5 {
            super_block_offset: 0,
            super_block_size: 8192,
            inode_size: 128,
            chunk_info_size: 80,
        }
    }
}

pub enum Action {
    Break,
    Continue,
}

impl RafsInspector {
    pub fn new(b: &Path, request_mode: bool) -> Result<Self> {
        let layout_profile = RafsLayoutV5::new();
        let mut f = <dyn RafsIoRead>::from_file(b)
            .map_err(|e| anyhow!("Can't find bootstrap, path={:?}, {:?}", b, e))?;
        let sb = Self::super_block(&mut f, &layout_profile)?;
        let rafs_meta: RafsMeta = (&sb).into();

        let mut inodes_table = RafsV5InodeTable::new(rafs_meta.inode_table_entries as usize);
        f.seek_to_offset(rafs_meta.inode_table_offset)?;
        inodes_table.load(&mut f)?;

        f.seek_to_offset(rafs_meta.blob_table_offset)?;
        let mut blobs_table = RafsV5BlobTable::new();
        blobs_table.load(&mut f, rafs_meta.blob_table_size)?;

        // Load extended blob table if the bootstrap including
        // extended blob table.
        let extended_blobs_table = if rafs_meta.extended_blob_table_offset > 0 {
            f.seek_to_offset(rafs_meta.extended_blob_table_offset)?;
            let mut et = RafsV5ExtBlobTable::new();
            et.load(&mut f, rafs_meta.extended_blob_table_entries as usize)?;
            Some(et)
        } else {
            None
        };

        Ok(RafsInspector {
            request_mode,
            bootstrap: Arc::new(Mutex::new(f)),
            layout_profile,
            rafs_meta,
            // Root inode has index of 0
            cur_dir_index: 0,
            parent_indexes: Vec::new(),
            inodes_table,
            blobs_table,
            extended_blobs_table,
        })
    }

    fn super_block(
        b: &mut RafsIoReader,
        layout_profile: &RafsLayoutV5,
    ) -> Result<RafsV5SuperBlock> {
        let mut sb = RafsV5SuperBlock::new();

        b.seek_to_offset(layout_profile.super_block_offset as u64)?;
        sb.load(b)
            .map_err(|e| anyhow!("Failed in loading super block, {:?}", e))?;

        Ok(sb)
    }

    fn load_ondisk_inode(&self, offset: u32) -> Result<(RafsV5Inode, OsString)> {
        let mut ondisk_inode = RafsV5Inode::new();
        let mut guard = self.bootstrap.lock().unwrap();
        let bootstrap = guard.deref_mut();
        bootstrap.seek_to_offset(offset as u64)?;
        ondisk_inode
            .load(bootstrap)
            .map_err(|e| anyhow!("failed to jump to inode offset={}, {:?}", offset, e))?;

        // No need to move offset forward
        let file_name = ondisk_inode.file_name(bootstrap)?;

        Ok((ondisk_inode, file_name))
    }

    /// Index is u32, by which the inode can be found.
    fn load_inode_by_index(&self, index: usize) -> Result<(RafsV5Inode, OsString)> {
        // Safe to truncate `inode_table_offset` now.
        let inode_offset = self.inodes_table.data[index] << 3;
        self.load_ondisk_inode(inode_offset)
    }

    fn list_chunks(
        r: &mut RafsIoReader,
        inode: &RafsV5Inode,
        inode_offset: u32,
    ) -> Result<Option<Vec<RafsV5ChunkInfo>>> {
        if !inode.is_reg() {
            return Ok(None);
        }

        let mut xattr_pairs_aligned_size = 0u32;
        let mut chunks = None;

        if inode.has_xattr() {
            let xattr_header_offset = inode_offset + inode.size() as u32;
            r.seek_to_offset(xattr_header_offset as u64)?;
            // TODO: implement `load()` for `OndiskXattr`
            let mut xattrs_header = RafsV5XAttrsTable::new();
            r.read_exact(xattrs_header.as_mut())?;
            xattr_pairs_aligned_size = xattrs_header.aligned_size() as u32 + 8;
        }

        let chunks_offset = inode_offset + inode.size() as u32 + xattr_pairs_aligned_size;

        r.seek_to_offset(chunks_offset as u64)?;

        if inode.i_child_count > 0 {
            chunks = Some(Vec::<RafsV5ChunkInfo>::new());
            for _ in 0..inode.i_child_count {
                let mut chunk = RafsV5ChunkInfo::new();
                chunk.load(r)?;
                chunks.as_mut().unwrap().push(chunk);
            }
        }

        Ok(chunks)
    }

    fn stat_single_file(inode: &RafsV5Inode, name: &str, index: usize) {
        println!(
            r#"
Inode Number:       {inode_number}
Index:              {index}
Name:               {name:?}
Size:               {size}
Parent:             {parent}
Mode:               0x{mode:X}
Permissions:        {permissions:o}
Nlink:              {nlink}
UID:                {uid}
GID:                {gid}
Mtime:              {mtime}
MtimeNsec:          {mtime_nsec}
Blocks:             {blocks}"#,
            inode_number = inode.i_ino,
            name = name,
            index = index,
            size = inode.i_size,
            parent = inode.i_parent,
            mode = inode.i_mode,
            permissions = Permissions::from_mode(inode.i_mode).mode(),
            nlink = inode.i_nlink,
            uid = inode.i_uid,
            gid = inode.i_gid,
            mtime = inode.i_mtime,
            mtime_nsec = inode.i_mtime_nsec,
            blocks = inode.i_blocks,
        );
    }

    pub fn iter_dir(
        &self,
        mut op: impl FnMut(&OsStr, &RafsV5Inode, u32, u32) -> Action,
    ) -> Result<()> {
        let (dir_inode, _) = self.load_inode_by_index(self.cur_dir_index as usize)?;
        let parent_ino = dir_inode.i_ino;

        let children_count = dir_inode.i_child_count;
        // Somehow, the it has subtract 1 to identify the first child file's index in inode table.
        let first_index = dir_inode.i_child_index - 1;
        let last_index = first_index + children_count - 1;

        for idx in first_index..=last_index {
            let (child_inode, name) = self.load_inode_by_index(idx as usize)?;

            if child_inode.i_parent != parent_ino {
                bail!("File {:?} is not a child of CWD", name);
            }

            trace!("inode: {:?}; name: {:?}", child_inode, name);
            let inode_offset = self.inodes_table.data[idx as usize] << 3;
            match op(name.as_os_str(), &child_inode, idx, inode_offset) {
                Action::Break => break,
                Action::Continue => continue,
            }
        }

        Ok(())
    }

    fn walk_fs(
        &self,
        top_index: u32,
        op: &mut dyn FnMut(&OsStr, &RafsV5Inode, u32, u32) -> Action,
    ) -> Result<()> {
        let (top, _) = self.load_inode_by_index(top_index as usize)?;
        let parent_ino = top.i_ino;
        let mut dir_indexes = vec![];

        let children_count = top.i_child_count;
        // Somehow, the it has subtract 1 to identify the first child file's index in inode table.
        let first_index = top.i_child_index - 1;
        let last_index = first_index + children_count - 1;

        for idx in first_index..=last_index {
            let (child_inode, name) = self.load_inode_by_index(idx as usize)?;

            if child_inode.i_parent != parent_ino {
                bail!("File {:?} is not a child of CWD", name);
            }

            if child_inode.is_dir() {
                dir_indexes.push(idx);
            }

            trace!("inode: {:?}; name: {:?}", child_inode, name);
            let inode_offset = self.inodes_table.data[idx as usize] << 3;
            match op(name.as_os_str(), &child_inode, idx, inode_offset) {
                Action::Break => break,
                Action::Continue => continue,
            }
        }

        for i in dir_indexes {
            self.walk_fs(i, op)?;
        }

        Ok(())
    }

    fn path_from_ino(&self, mut ino: u64) -> Result<PathBuf> {
        let mut path = PathBuf::new();
        let mut entries = Vec::<PathBuf>::new();

        loop {
            let offset = self.inodes_table.get(ino)?;
            let (inode, file_name) = self.load_ondisk_inode(offset)?;
            entries.push(file_name.into());
            if inode.i_parent == 0 {
                break;
            }
            ino = inode.i_parent;
        }
        entries.reverse();
        for e in entries {
            path.push(e);
        }

        Ok(path)
    }

    pub fn cmd_show_chunk(&self, offset_in_blob: u64) -> Result<Option<Value>> {
        let b = self.bootstrap.clone();
        self.walk_fs(0, &mut |name, inode, _index, offset| {
            // Not expect poisoned lock
            let mut guard = b.lock().unwrap();
            let bootstrap = &mut *guard;

            // Only regular file has data chunks.
            if !inode.is_reg() {
                return Action::Continue;
            }

            if let Ok(Some(chunks)) = Self::list_chunks(bootstrap, inode, offset) {
                drop(guard);
                for c in chunks {
                    if c.compress_offset == offset_in_blob {
                        let path = self.path_from_ino(inode.i_parent).unwrap();
                        println!(
                            r#"
    {:width$} Parent Path {:width$}
    Chunk ID: {:50}, Blob ID: {}
"#,
                            name.to_string_lossy(),
                            path.to_string_lossy(),
                            c.block_id,
                            if let Ok(ref blob) = self.blobs_table.get(c.blob_index) {
                                &blob.blob_id
                            } else {
                                error!("Can't find blob by its index, index={:?}", c.blob_index);
                                return Action::Break;
                            },
                            width = 32
                        );
                    }
                }
            } else {
                return Action::Break;
            }
            Action::Continue
        })?;

        Ok(None)
    }

    fn cmd_check_inode(&self, ino: u64) -> Result<Option<Value>> {
        self.walk_fs(0, &mut |name, inode, index, _offset| {
            // Not expect poisoned lock
            if inode.i_ino == ino {
                println!(
                    r#"{}"#,
                    self.path_from_ino(inode.i_ino).unwrap().to_string_lossy(),
                );
                Self::stat_single_file(inode, &name.to_string_lossy(), index as usize);
            }

            Action::Continue
        })?;

        Ok(None)
    }

    fn cmd_stat_file_by_index(&self, index: usize) -> Result<Option<Value>> {
        let (inode, name) = self.load_inode_by_index(index)?;
        Self::stat_single_file(&inode, &name.to_string_lossy(), index);
        Ok(None)
    }

    pub fn cmd_list_dir(&mut self) -> Result<Option<Value>> {
        self.iter_dir(|f, inode, _idx, _offset| {
            trace!("inode {:?}, name: {:?}", inode, f);

            let sign = if inode.is_reg() {
                "-"
            } else if inode.is_dir() {
                "d"
            } else if inode.is_symlink() {
                "l"
            } else {
                " "
            };

            println!(
                r#"{}    {inode_number:<8} {name:?}"#,
                sign,
                name = f,
                inode_number = inode.i_ino,
            );

            Action::Continue
        })?;

        Ok(None)
    }

    fn cmd_list_prefetch(&mut self) -> Result<Option<Value>> {
        let mut pt = RafsV5PrefetchTable::new();
        let mut guard = self.bootstrap.lock().unwrap();
        let bootstrap = guard.deref_mut();
        pt.load_prefetch_table_from(
            bootstrap,
            self.rafs_meta.prefetch_table_offset,
            self.rafs_meta.prefetch_table_entries as usize,
        )?;

        drop(guard);

        let o = if self.request_mode {
            let mut value = json!([]);
            for ino in pt.inodes {
                let path = self.path_from_ino(ino as u64)?;
                let v = json!({"inode":ino, "path": path});
                value.as_array_mut().unwrap().push(v);
            }
            Some(value)
        } else {
            println!(
                "Prefetched Files: {}",
                self.rafs_meta.prefetch_table_entries
            );
            for ino in pt.inodes {
                let path = self.path_from_ino(ino as u64)?;
                println!(
                    r#"Inode Number:{inode_number:10} | Path: {path:?} "#,
                    path = path,
                    inode_number = ino,
                );
            }
            None
        };

        Ok(o)
    }

    pub fn cmd_stat_file(&self, name: &str) -> Result<Option<Value>> {
        let b = self.bootstrap.clone();

        if name == "." {
            let (dir_inode, name) = self.load_inode_by_index(self.cur_dir_index as usize)?;
            Self::stat_single_file(
                &dir_inode,
                &name.to_string_lossy(),
                self.cur_dir_index as usize,
            );
            return Ok(None);
        }

        self.iter_dir(|f, inode, idx, offset| {
            if f == name {
                let mut guard = b.lock().unwrap();
                let bootstrap = guard.deref_mut();
                let chunks = Self::list_chunks(bootstrap, inode, offset);
                Self::stat_single_file(inode, name, idx as usize);

                if let Ok(Some(cks)) = chunks {
                    println!("    Chunks list:");
                    for (i, c) in cks.iter().enumerate() {
                        let blob_id = if let Ok(entry) = self.blobs_table.get(c.blob_index) {
                            entry.blob_id.clone()
                        } else {
                            error!(
                                "Blob index is {} . But no blob entry associate with it",
                                c.blob_index
                            );
                            return Action::Break;
                        };

                        println!(
                            r#"        {} ->
            file offset: {file_offset}, chunk index: {chunk_index}
            compressed size: {compressed_size}, decompressed size: {decompressed_size}
            compressed offset: {compressed_offset}, decompressed offset: {decompressed_offset},
            blob id: {blob_id}, chunk id: {chunk_id}
        "#,
                            i,
                            chunk_index = c.index,
                            file_offset = c.file_offset,
                            compressed_size = c.compress_size,
                            decompressed_size = c.decompress_size,
                            decompressed_offset = c.decompress_offset,
                            compressed_offset = c.compress_offset,
                            blob_id = blob_id,
                            chunk_id = c.block_id
                        );
                    }
                }

                return Action::Break;
            }
            Action::Continue
        })?;

        Ok(None)
    }

    pub fn cmd_change_dir(&mut self, name: &str) -> Result<Option<Value>> {
        if name == "." {
            return Ok(None);
        }

        if name == ".." {
            if let Some(p) = self.parent_indexes.pop() {
                self.cur_dir_index = p
            }
            return Ok(None);
        }

        // let path: PathBuf = name.to_string().into();
        // let entries = path.components();

        let mut new_dir_index = None;
        let mut err = "File does not exist";

        self.iter_dir(|f, i, idx, _offset| {
            if f == name {
                if i.is_dir() {
                    new_dir_index = Some(idx);
                    return Action::Break;
                } else {
                    err = "Not a directory";
                    return Action::Break;
                }
            }
            Action::Continue
        })?;

        if let Some(n) = new_dir_index {
            self.parent_indexes.push(self.cur_dir_index);
            self.cur_dir_index = n;
        } else {
            println!("{}", err);
        }

        Ok(None)
    }

    pub fn cmd_stats(&mut self) -> Result<Option<Value>> {
        let mut guard = self.bootstrap.lock().unwrap();
        let bootstrap = guard.deref_mut();
        let sb = Self::super_block(bootstrap, &self.layout_profile)?;

        let o = if self.request_mode {
            Some(json!({"inodes_count": sb.inodes_count()}))
        } else {
            println!(
                r#"
    Version:            {version}
    Inodes Count:       {inodes_count}
    Flags:              {flags}"#,
                version = sb.version(),
                inodes_count = sb.inodes_count(),
                flags = RafsSuperFlags::from_bits(sb.flags()).unwrap()
            );

            None
        };

        Ok(o)
    }

    pub fn cmd_list_blobs(&mut self) -> Result<Option<Value>> {
        let mut guard = self.bootstrap.lock().unwrap();
        let bootstrap = guard.deref_mut();
        bootstrap.seek_to_offset(self.rafs_meta.blob_table_offset)?;

        let blobs = &mut self.blobs_table;
        let extended = &mut self.extended_blobs_table;

        let o = if self.request_mode {
            let mut value = json!([]);

            for (i, b) in blobs.entries.iter().enumerate() {
                let (decompressed_size, compressed_size) = if let Some(et) = extended {
                    (
                        Some(et.entries[i].blob_cache_size),
                        Some(et.entries[i].compressed_blob_size),
                    )
                } else {
                    (None, None)
                };

                let v = json!({"blob_id": b.blob_id, "readahead_offset": b.readahead_offset,
                "readahead_size":b.readahead_size, "decompressed_size": decompressed_size, "compressed_size": compressed_size});
                value.as_array_mut().unwrap().push(v);
            }
            Some(value)
        } else {
            for (i, b) in blobs.entries.iter().enumerate() {
                print!(
                    r#"
    Blob ID:            {blob_id}
    Readahead Offset:   {readahead_offset}
    Readahead Size:     {readahead_size}
    "#,
                    blob_id = b.blob_id,
                    readahead_offset = b.readahead_offset,
                    readahead_size = b.readahead_size,
                );

                if let Some(et) = extended {
                    print!(
                        r#"Cache Size:         {cache_size}
    Compressed Size:    {compressed_size}
    "#,
                        cache_size = et.entries[i].blob_cache_size,
                        compressed_size = et.entries[i].compressed_blob_size
                    )
                }
            }
            None
        };

        Ok(o)
    }
}

pub(crate) struct Prompt {}

#[derive(Debug)]
pub(crate) enum ExecuteError {
    HelpCommand,
    IllegalCommand,
    ArgumentParse,
    Exit,
    ExecuteError(anyhow::Error),
}

pub(crate) struct Executor {}

impl Executor {
    pub fn execute(
        inspector: &mut RafsInspector,
        input: String,
    ) -> std::result::Result<Option<Value>, ExecuteError> {
        let mut raw = input.strip_suffix("\n").unwrap_or(&input).split(' ');
        let cmd = raw.next().unwrap();
        let args = raw.next();

        debug!("execute {:?} {:?}", cmd, args);

        let output = match (cmd, args) {
            ("help", _) => {
                Self::usage();
                return Err(ExecuteError::HelpCommand);
            }
            ("exit", _) | ("q", _) => return Err(ExecuteError::Exit),
            ("stats", None) => inspector.cmd_stats(),
            ("ls", None) => inspector.cmd_list_dir(),
            ("cd", Some(dir)) => inspector.cmd_change_dir(dir),
            ("stat", Some(file_name)) => inspector.cmd_stat_file(file_name),
            ("blobs", None) => inspector.cmd_list_blobs(),
            ("prefetch", None) => inspector.cmd_list_prefetch(),
            ("chunk", Some(argument)) => {
                let offset: u64 = argument.parse().unwrap();
                inspector.cmd_show_chunk(offset)
            }
            ("icheck", Some(argument)) => {
                let ino: u64 = argument.parse().map_err(|_| {
                    println!("Wrong INODE is specified. Is it a inode number?");
                    ExecuteError::ArgumentParse
                })?;
                inspector.cmd_check_inode(ino)
            }
            ("index", Some(argument)) => {
                let index: usize = argument.parse().map_err(|_| {
                    println!("Wrong INDEX is specified. Is it an integer?");
                    ExecuteError::ArgumentParse
                })?;
                inspector.cmd_stat_file_by_index(index)
            }
            _ => {
                println!("Unsupported command!");
                {
                    Self::usage();
                    return Err(ExecuteError::IllegalCommand);
                };
            }
        }
        .map_err(ExecuteError::ExecuteError)?;

        Ok(output)
    }

    pub(crate) fn usage() {
        println!(
            r#"
    stats:              Display global rafs metadata
    ls:                 Show files in current directory
    cd DIR:             Change current directory
    stat FILE_NAME:     Show particular information of rafs inode
    blobs:              Show blobs table
    prefetch:           Show prefetch table
    chunk OFFSET:       List basic info of a single chunk together with a list of files that share it
    icheck INODE:       Show path of the inode and basic information
    index INDEX:        Show information about a file by its index
        "#
        );
    }
}

impl Prompt {
    pub(crate) fn run(mut inspector: RafsInspector) {
        loop {
            print!("Inspecting Rafs :> ");
            std::io::stdout().flush().unwrap();

            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap();

            match Executor::execute(&mut inspector, input) {
                Err(ExecuteError::Exit) => break,
                Err(ExecuteError::IllegalCommand) => continue,
                Err(ExecuteError::HelpCommand) => continue,
                Err(ExecuteError::ExecuteError(e)) => {
                    println!("Failed in executing command, {:?}", e);
                    continue;
                }
                Ok(Some(o)) => {
                    serde_json::to_writer(std::io::stdout(), &o)
                        .unwrap_or_else(|e| error!("Failed to serialize, {:?}", e));
                }
                _ => continue,
            }
        }
    }
}

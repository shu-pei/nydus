use std::collections::HashMap;

use crate::core::context::BlobContext;

use rafs::metadata::layout::v5::RafsV5BlobTable;
use rafs::metadata::layout::v5::RafsV5ChunkInfo;
// FIXME: Must image tool depend on storage backend?
use nydus_utils::digest::RafsDigest;

pub struct BlobInfo {
    blobs: Vec<BlobContext>,
    /// Store all chunk digest for chunk deduplicate during build.
    pub chunk_map: HashMap<RafsDigest, RafsV5ChunkInfo>,
}

impl BlobInfo {
    pub fn new() -> Self {
        Self {
            blobs: Vec::new(),
            chunk_map: HashMap::new(),
        }
    }
    pub fn from_blob_table(&mut self, blob_table: &RafsV5BlobTable) {
        self.blobs = blob_table
            .get_all()
            .iter()
            .map(|entry| {
                BlobContext::from(
                    entry.blob_id.clone(),
                    entry.chunk_count,
                    entry.readahead_size,
                    entry.blob_cache_size,
                    entry.compressed_blob_size,
                )
            })
            .collect();
    }
    pub fn print_blob_ids(&self) {
        for blob_context in &self.blobs {
            println!("Blob ID: {:?}", blob_context.blob_id);
        }
    }
    pub fn print_sizes(&self) {
        println!("blobs size: {}", self.blobs.len());
        println!("chunk_map size: {}", self.chunk_map.len());
    }
}

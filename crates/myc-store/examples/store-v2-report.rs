//! Report blob-layer (v1) and chunk-layer (v2) statistics for a store root.
//!
//! Used by the store-v2 measurements (`docs/design/store-v2.md`):
//!
//! ```text
//! cargo run -p myc-store --example store-v2-report -- /path/to/store
//! ```

fn main() {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| usage("missing <store-root> argument"));
    let store = myc_store::Store::open(&root).unwrap_or_else(|e| usage(&e.to_string()));
    let blobs = store.blob_stats().expect("blob stats");
    let chunks = store.chunk_stats().expect("chunk stats");
    println!("blobs {} {}", blobs.count, blobs.bytes);
    println!("packs {} {}", chunks.packs, chunks.pack_disk_bytes);
    println!(
        "unique_chunks {} {}",
        chunks.unique_chunks, chunks.unique_chunk_bytes
    );
    println!("sidecars {} {}", chunks.sidecars, chunks.sidecar_bytes);
    println!(
        "total_disk {}",
        blobs.bytes + chunks.pack_disk_bytes + chunks.sidecar_bytes
    );
}

fn usage(msg: &str) -> ! {
    eprintln!("store-v2-report: {msg}\nusage: store-v2-report <store-root>");
    std::process::exit(2);
}

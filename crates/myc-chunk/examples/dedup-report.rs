//! Compare des fichiers deux à deux : octets partagés au niveau FICHIER
//! (égalité binaire stricte, la seule déduplication que fait mycel v1) contre
//! octets partagés au niveau BLOC (chunks content-defined partagés, le
//! comportement visé par un futur format v2).
//!
//! Usage : `dedup-report <FICHIER>...`
//!
//! Pour chaque paire (A, B), rapporte combien d'octets de B sont couverts par
//! des blocs dont le hash BLAKE3 apparaît aussi dans A — c'est-à-dire ce
//! qu'un store par blocs n'aurait pas besoin de réécrire après avoir stocké
//! A — et le compare au niveau fichier.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::process::ExitCode;

use myc_chunk::{chunk_bounds, chunk_hashes};

struct FileReport {
    path: String,
    data: Vec<u8>,
    bounds: Vec<(usize, usize)>,
    hashes: Vec<[u8; 32]>,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: dedup-report <FICHIER>...");
        return ExitCode::from(2);
    }

    let mut files = Vec::new();
    for path in &args {
        match fs::read(path) {
            Ok(data) => {
                let bounds = chunk_bounds(&data);
                let hashes = chunk_hashes(&data);
                files.push(FileReport {
                    path: path.clone(),
                    data,
                    bounds,
                    hashes,
                });
            }
            Err(e) => {
                eprintln!("dedup-report: {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    for f in &files {
        print_file_stats(f);
    }
    for i in 0..files.len() {
        for j in (i + 1)..files.len() {
            print_pair(&files[i], &files[j]);
        }
    }
    ExitCode::SUCCESS
}

fn print_file_stats(f: &FileReport) {
    let unique: HashSet<&[u8; 32]> = f.hashes.iter().collect();
    let sizes: Vec<usize> = f.bounds.iter().map(|&(s, e)| e - s).collect();
    let min = sizes.iter().min().copied().unwrap_or(0);
    let max = sizes.iter().max().copied().unwrap_or(0);
    let mean = if sizes.is_empty() {
        0.0
    } else {
        f.data.len() as f64 / sizes.len() as f64
    };
    println!("fichier   : {}", f.path);
    println!(
        "  taille  : {} octets, {} blocs (min {min}, moy {mean:.0}, max {max}), {} blocs uniques",
        f.data.len(),
        f.bounds.len(),
        unique.len()
    );
}

fn print_pair(a: &FileReport, b: &FileReport) {
    println!();
    println!("paire     : {}  vs  {}", a.path, b.path);

    // Niveau FICHIER : la dédup v1 ne partage que l'égalité binaire exacte.
    let file_common = if a.data == b.data { b.data.len() } else { 0 };

    // Niveau BLOC : octets de B couverts par des blocs déjà présents dans A.
    let a_set: HashSet<&[u8; 32]> = a.hashes.iter().collect();
    let mut block_common = 0usize;
    let mut shared_chunks = 0usize;
    for (&hash, &(s, e)) in b.hashes.iter().zip(&b.bounds) {
        if a_set.contains(&hash) {
            block_common += e - s;
            shared_chunks += 1;
        }
    }

    let pct = |n: usize| -> String {
        if b.data.is_empty() {
            "n/a".to_string()
        } else {
            format!("{:.2}%", 100.0 * n as f64 / b.data.len() as f64)
        }
    };
    println!(
        "  niveau FICHIER : {:>10} octets de B en commun ({})",
        file_common,
        pct(file_common)
    );
    println!(
        "  niveau BLOC    : {:>10} octets de B en commun ({}, {}/{} blocs)",
        block_common,
        pct(block_common),
        shared_chunks,
        b.bounds.len()
    );
    if file_common == 0 {
        println!("  ratio bloc/fichier : infini (fichier : 0 octet, bloc : {block_common} octets)");
    } else {
        println!(
            "  ratio bloc/fichier : {:.2}x",
            block_common as f64 / file_common as f64
        );
    }
}

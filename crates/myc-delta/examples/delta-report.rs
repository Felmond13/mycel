//! Compare, sur une paire de fichiers réels, la compression DELTA (ce crate)
//! avec la déduplication par blocs CDC (`myc-chunk`) : les deux chiffres
//! côte à côte.
//!
//! Usage : `delta-report SOURCE CIBLE` — SOURCE = ancienne version déjà
//! stockée, CIBLE = nouvelle version à transmettre/stocker.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::process::ExitCode;

use myc_chunk::{chunk_bounds, chunk_hashes};
use myc_delta::{apply, delta, delta_stats};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: delta-report SOURCE CIBLE");
        return ExitCode::from(2);
    }
    let (source, target) = match (fs::read(&args[0]), fs::read(&args[1])) {
        (Ok(s), Ok(t)) => (s, t),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("delta-report: lecture impossible : {e}");
            return ExitCode::FAILURE;
        }
    };

    let d = delta(&source, &target);
    let stats = match delta_stats(&d) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("delta-report: delta auto-produite invalide : {e}");
            return ExitCode::FAILURE;
        }
    };
    let ok = matches!(apply(&source, &d), Ok(ref out) if *out == target);

    // Dédup par blocs : octets de la cible couverts par des chunks CDC dont
    // le hash BLAKE3 existe déjà dans la source (métrique de dedup-report).
    let src_hashes: HashSet<[u8; 32]> = chunk_hashes(&source).into_iter().collect();
    let t_bounds = chunk_bounds(&target);
    let mut shared = 0usize;
    for (&h, &(s, e)) in chunk_hashes(&target).iter().zip(&t_bounds) {
        if src_hashes.contains(&h) {
            shared += e - s;
        }
    }

    let tlen = target.len().max(1) as f64;
    let pct = |n: usize| 100.0 * n as f64 / tlen;
    println!("source         : {} ({} octets)", args[0], source.len());
    println!("cible          : {} ({} octets)", args[1], target.len());
    println!(
        "delta          : {} octets ({:.2} % de la cible)",
        d.len(),
        pct(d.len())
    );
    println!(
        "  COPY (MATCH) : {:>9} octets ({:6.2} %) en {} opérations",
        stats.copy_bytes,
        pct(stats.copy_bytes as usize),
        stats.copy_ops
    );
    println!(
        "  LIT          : {:>9} octets ({:6.2} %) en {} opérations",
        stats.lit_bytes,
        pct(stats.lit_bytes as usize),
        stats.lit_ops
    );
    println!(
        "round-trip     : {}",
        if ok { "OK (apply == cible)" } else { "ÉCHEC" }
    );
    println!(
        "dédup blocs    : {:>9} octets de la cible couverts ({:6.2} %)",
        shared,
        pct(shared)
    );
    let copy = stats.copy_bytes as usize;
    if shared == 0 {
        println!("delta / blocs  : delta capte {copy} octets là où les blocs n'en captent aucun");
    } else {
        println!(
            "delta / blocs  : delta couvre {:.1}x plus d'octets que la dédup par blocs",
            copy as f64 / shared as f64
        );
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

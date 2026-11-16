use std::path::PathBuf;

use anyhow::{Result, bail};

use archix::journal::{read, verify, writemap};

pub struct LogArgs {
    pub n: Option<u32>,
    pub verify: bool,
    pub print_allowlist: bool,
    pub pacman_log: Option<PathBuf>,
    pub dbpath: Option<PathBuf>,
}

/// `archix log [N] [--verify]`: read-only, so it runs as any user who can read the journal; returns the exit code.
pub fn log(args: &LogArgs) -> Result<u8> {
    let root = PathBuf::from("/");
    if args.print_allowlist {
        print!("{}", writemap::render());
        if args.n.is_none() {
            return Ok(0);
        }
    }
    if args.verify {
        let Some(n) = args.n else {
            bail!("`archix log --verify` needs the switch number: archix log --verify N");
        };
        let sources = verify::Sources {
            journal_root: root,
            pacman_log: args.pacman_log.clone(),
            dbpath: args.dbpath.clone(),
            ..Default::default()
        };
        let report = verify::verify(n, &sources, &verify::SystemRunner)?;
        print!("{}", report.render());
        return Ok(if report.pass() { 0 } else { 1 });
    }
    match args.n {
        Some(n) => print!("{}", read::render_one(&root, n)?),
        None => print!("{}", read::render_list(&root)?),
    }
    Ok(0)
}
